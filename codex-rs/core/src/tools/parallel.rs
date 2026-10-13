use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::RwLock;
use tokio::task::JoinError;
use tokio_util::either::Either;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;

use crate::function_tool::FunctionCallError;
use crate::function_tool::OrCancelToolExt;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::call_trace;
use crate::tools::context::AbortedToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolCallState;
use crate::tools::context::ToolPayload;
use crate::tools::lifecycle::notify_tool_aborted;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::models::ResponseInputItem;

struct ToolCallTimingGuard {
    started_at: Option<Instant>,
    execution_started_at: Arc<OnceLock<Instant>>,
    conversation_id: String,
    turn_id: String,
    call_id: String,
    tool_name: codex_tools::ToolName,
    extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
}

#[derive(Clone)]
pub(crate) struct ToolCallRuntime {
    session: Arc<Session>,
    // Tool calls may run later, so retain the step whose tool list advertised them.
    step_context: Arc<StepContext>,
    tracker: SharedTurnDiffTracker,
    parallel_execution: Arc<RwLock<()>>,
}

impl ToolCallRuntime {
    pub(crate) fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Self {
        Self {
            session,
            step_context,
            tracker,
            parallel_execution: Arc::new(RwLock::new(())),
        }
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &codex_tools::ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.step_context
            .tool_router
            .create_diff_consumer(tool_name)
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call(
        self,
        call: ToolCall,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Result<ResponseItemEnvelope, CodexErr>> {
        let error_call = call.clone();
        let source = call.direct_source();
        let recorder = self.session.services.executed_tool_calls.clone();
        let recorded_call = recorder.prepare_direct_call(&call);
        let step_context = Arc::clone(&self.step_context);
        let call_state = Arc::new(ToolCallState::default());
        let future = self.handle_tool_call_with_source(
            step_context,
            call,
            source,
            cancellation_token,
            Arc::clone(&call_state),
        );
        async move {
            let result = future.await;
            let recorded_call = recorded_call.filter(|(_, recording)| recording.strong_count() > 0);
            let mut response = match result {
                Ok(result) => result.into_response(),
                Err(FunctionCallError::Fatal(message)) => return Err(CodexErr::Fatal(message)),
                Err(other) => {
                    ResponseItemEnvelope::new(Self::failure_response(error_call, other).into())
                }
            };
            recorder.attach_direct_call_to_output(&mut response.item, recorded_call);
            Ok(response)
        }
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) fn handle_tool_call_with_source(
        self,
        step_context: Arc<StepContext>,
        call: ToolCall,
        source: ToolCallSource,
        cancellation_token: CancellationToken,
        call_state: Arc<ToolCallState>,
    ) -> impl std::future::Future<Output = Result<AnyToolResult, FunctionCallError>> {
        let router = &step_context.tool_router;
        let supports_parallel = router.tool_supports_parallel(&call);
        let tool_runtime = router
            .registered_tool(&call.tool_name)
            .map(|tool| Arc::clone(&tool.runtime));
        let finishes_on_cancellation = tool_runtime
            .as_ref()
            .is_some_and(|runtime| runtime.finishes_on_cancellation());
        let router = Arc::clone(router);
        let session = Arc::clone(&self.session);
        let turn = Arc::clone(&step_context.turn);
        let tracker = Arc::clone(&self.tracker);
        let lock = Arc::clone(&self.parallel_execution);
        let invocation_cancellation_token = cancellation_token.clone();
        let started = Instant::now();
        let tool_call_timing_guard = ToolCallTimingGuard::capture(
            started,
            &session.thread_id,
            &turn.sub_id,
            &call,
            Arc::clone(&session.services.extensions),
        );
        for observer in session.services.extensions.tool_lifecycle_contributors() {
            observer.on_tool_dispatch(codex_extension_api::ToolDispatchInput {
                thread_id: &tool_call_timing_guard.conversation_id,
                turn_id: &tool_call_timing_guard.turn_id,
                call_id: &tool_call_timing_guard.call_id,
                tool_name: &tool_call_timing_guard.tool_name,
            });
        }
        let execution_started_at = Arc::clone(&tool_call_timing_guard.execution_started_at);
        let abort_session = Arc::clone(&session);
        let abort_turn = Arc::clone(&turn);
        let dispatch_call_state = Arc::clone(&call_state);
        let dispatch_call = call.clone();
        let thread_id = session.thread_id;
        let dispatch_tool_name = call.tool_name.clone();
        let dispatch_call_id = call.call_id.clone();

        let dispatch_span = trace_span!(
            "dispatch_tool_call",
            otel.name = %call.tool_name,
            tool_name = %call.tool_name,
            thread.id = %session.thread_id,
            call_id = call.call_id.as_str(),
            aborted = false,
        );
        let abort_dispatch_span = dispatch_span.clone();

        let mut dispatch_handle = AbortOnDropHandle::new(tokio::spawn(
            async move {
                // Readiness and admission remain cancellable even when the handler must finish.
                let guard = async {
                    if let Some(tool_runtime) = tool_runtime
                        && let Some(readiness) = tool_runtime.wait_until_ready(&session)
                    {
                        readiness.await;
                    }

                    if supports_parallel {
                        Either::Left(lock.read().await)
                    } else {
                        Either::Right(lock.write().await)
                    }
                }
                .or_cancel_tool(&invocation_cancellation_token)
                .await?;
                // Admission through the parallel-execution gate marks the end
                // of dispatch waiting and the start of handler execution.
                let _ = execution_started_at.set(Instant::now());

                let result = router
                    .dispatch_tool_call_with_state(
                        session,
                        step_context,
                        invocation_cancellation_token,
                        tracker,
                        dispatch_call,
                        source,
                        dispatch_call_state,
                    )
                    .instrument(dispatch_span.clone())
                    .await;
                drop(guard);
                // The sampling loop collects results in order only after its stream ends.
                // Record readiness here, before either caller encodes or collects the result.
                // A fatal error still propagates to the caller instead of producing a tool
                // result; unlike a normal tool failure, it has no readiness event.
                if !matches!(&result, Err(FunctionCallError::Fatal(_))) {
                    call_trace::result_ready(
                        thread_id,
                        &turn.sub_id,
                        &dispatch_tool_name,
                        &dispatch_call_id,
                    );
                }
                result
            }
            .in_current_span(),
        ));

        async move {
            let mut tool_call_timing_guard = tool_call_timing_guard;
            let mut result = tokio::select! {
                res = &mut dispatch_handle => res.map_err(Self::tool_task_join_error)?,
                _ = cancellation_token.cancelled() => {
                    // Dynamic handlers observe the token and publish their terminal item themselves.
                    if finishes_on_cancellation
                        || call_state.terminal_outcome_reached.load(Ordering::Acquire)
                        || dispatch_handle.is_finished()
                    {
                        dispatch_handle.await.map_err(Self::tool_task_join_error)?
                    } else {
                        let secs = started.elapsed().as_secs_f32().max(0.1);
                        abort_dispatch_span.record("aborted", true);
                        dispatch_handle.abort();
                        match dispatch_handle.await {
                            Ok(result) => result,
                            Err(err) if err.is_cancelled() => {
                                let response = Self::aborted_response(&call, secs);
                                call_trace::result_ready(
                                    thread_id,
                                    &abort_turn.sub_id,
                                    &call.tool_name,
                                    &call.call_id,
                                );
                                call_state.terminal_outcome_reached.store(true, Ordering::Release);
                                notify_tool_aborted(
                                    abort_session.as_ref(),
                                    abort_turn.as_ref(),
                                    call.call_id.as_str(),
                                    &call.tool_name,
                                )
                                .await;
                                Ok(response)
                            }
                            Err(err) => Err(Self::tool_task_join_error(err)),
                        }
                    }
                },
            };
            // Cancellation outside the handler can return before the registry reports an outcome.
            if cancellation_token.is_cancelled()
                && !call_state
                    .terminal_outcome_reached
                    .swap(true, Ordering::AcqRel)
            {
                notify_tool_aborted(
                    abort_session.as_ref(),
                    abort_turn.as_ref(),
                    call.call_id.as_str(),
                    &call.tool_name,
                )
                .await;
                result = Ok(Self::aborted_response(
                    &call,
                    started.elapsed().as_secs_f32().max(0.1),
                ));
            }
            // Use one completion measurement for logging and response formatting.
            // Measuring inside a handler would omit routing and output processing.
            if let Some(handler_duration_ms) = tool_call_timing_guard.finish()
                && let Ok(result) = &mut result
            {
                result.result.set_handler_duration_ms(handler_duration_ms);
            }
            result
        }
        .in_current_span()
    }
}

impl ToolCallRuntime {
    fn tool_task_join_error(err: JoinError) -> FunctionCallError {
        FunctionCallError::Fatal(format!("tool task failed to receive: {err:?}"))
    }

    fn failure_response(call: ToolCall, err: FunctionCallError) -> ResponseInputItem {
        let message = err.to_string();
        match call.payload {
            ToolPayload::ToolSearch { .. } => ResponseInputItem::ToolSearchOutput {
                call_id: call.call_id,
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
            },
            ToolPayload::Custom { .. } => ResponseInputItem::CustomToolCallOutput {
                call_id: call.call_id,
                name: None,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
            _ => ResponseInputItem::FunctionCallOutput {
                call_id: call.call_id,
                output: codex_protocol::models::FunctionCallOutputPayload {
                    body: codex_protocol::models::FunctionCallOutputBody::Text(message),
                    success: Some(false),
                },
            },
        }
    }

    fn aborted_response(call: &ToolCall, secs: f32) -> AnyToolResult {
        AnyToolResult {
            call_id: call.call_id.clone(),
            payload: call.payload.clone(),
            result: Box::new(AbortedToolOutput {
                message: Self::abort_message(call, secs),
            }),
            post_tool_use_payload: None,
        }
    }

    fn abort_message(call: &ToolCall, secs: f32) -> String {
        if call.tool_name.is_default_namespace() && call.tool_name.name == "exec_command" {
            format!("Wall time: {secs:.1} seconds\naborted by user")
        } else {
            format!("aborted by user after {secs:.1}s")
        }
    }
}

impl ToolCallTimingGuard {
    fn capture(
        started_at: Instant,
        conversation_id: &impl std::fmt::Display,
        turn_id: &str,
        call: &ToolCall,
        extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
    ) -> Self {
        Self {
            started_at: Some(started_at),
            execution_started_at: Arc::new(OnceLock::new()),
            conversation_id: conversation_id.to_string(),
            turn_id: turn_id.to_string(),
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            extensions,
        }
    }

    /// Log call timing once and return the handler duration in milliseconds.
    fn finish(&mut self) -> Option<u64> {
        // Taking the start instant keeps normal completion and Drop from logging twice.
        let started_at = self.started_at.take()?;
        let completed_at = Instant::now();
        // Snapshot once so a concurrently-starting dispatch cannot make one
        // event internally inconsistent.
        let execution_started_at = self
            .execution_started_at
            .get()
            .copied()
            .filter(|execution_started_at| *execution_started_at <= completed_at);
        for observer in self.extensions.tool_lifecycle_contributors() {
            observer.on_tool_timing(codex_extension_api::ToolTimingInput {
                thread_id: &self.conversation_id,
                turn_id: &self.turn_id,
                call_id: &self.call_id,
                duration: execution_started_at
                    .map(|start| completed_at.duration_since(start))
                    .unwrap_or_default(),
            });
        }
        let duration_ms = |duration: std::time::Duration| u64::try_from(duration.as_millis()).ok();
        let total_duration_ms = duration_ms(completed_at.duration_since(started_at));
        let dispatch_duration_ms = execution_started_at.map_or_else(
            || total_duration_ms,
            |execution_started_at| duration_ms(execution_started_at.duration_since(started_at)),
        );
        let handler_duration_ms = execution_started_at.map_or(Some(0), |execution_started_at| {
            duration_ms(completed_at.duration_since(execution_started_at))
        });

        macro_rules! log_tool_call {
            ($dispatch_duration_ms:expr, $handler_duration_ms:expr, $total_duration_ms:expr) => {
                info!(
                    event.name = "codex.tool_call",
                    trace_id = %codex_otel::current_span_trace_id().unwrap_or_default(),
                    conversation.id = %self.conversation_id,
                    turn_id = %self.turn_id,
                    tool_name = %self.tool_name,
                    call_id = %self.call_id,
                    tool_source = "direct",
                    execution_started = execution_started_at.is_some(),
                    dispatch_duration_ms = $dispatch_duration_ms,
                    handler_duration_ms = $handler_duration_ms,
                    total_duration_ms = $total_duration_ms,
                    "tool call completed"
                );
            };
        }

        match (dispatch_duration_ms, handler_duration_ms, total_duration_ms) {
            (Some(dispatch_duration_ms), Some(handler_duration_ms), Some(total_duration_ms)) => {
                log_tool_call!(dispatch_duration_ms, handler_duration_ms, total_duration_ms);
            }
            _ => {
                log_tool_call!(
                    tracing::field::Empty,
                    tracing::field::Empty,
                    tracing::field::Empty
                );
            }
        }
        handler_duration_ms
    }
}

impl Drop for ToolCallTimingGuard {
    fn drop(&mut self) {
        // Cancellation can drop the outer future before its normal completion path runs.
        let _ = self.finish();
    }
}

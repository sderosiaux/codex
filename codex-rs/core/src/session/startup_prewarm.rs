//! Background model warmup shared by startup and idle-thread resume.
//! One scheduled task owns the prepared client session until the next regular
//! turn consumes it; shutdown and turn cancellation use the same handoff.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures::FutureExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::info;
use tracing::instrument;
use tracing::trace_span;
use tracing::warn;

use crate::client::ModelClientSession;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::RequestEffortUsage;
use crate::session::session::Session;
use crate::session::turn::build_prompt;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PrewarmInput {
    Base,
    History,
}

impl PrewarmInput {
    fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::History => "history",
        }
    }
}

pub(crate) struct SessionStartupPrewarmHandle {
    task: AbortOnDropHandle<CodexResult<ModelClientSession>>,
    started_at: Instant,
    timeout: Duration,
}

pub(crate) enum SessionStartupPrewarmResolution {
    Cancelled,
    Ready(Box<ModelClientSession>),
    Unavailable,
}

impl SessionStartupPrewarmHandle {
    pub(crate) fn new(
        task: JoinHandle<CodexResult<ModelClientSession>>,
        started_at: Instant,
        timeout: Duration,
    ) -> Self {
        Self {
            task: AbortOnDropHandle::new(task),
            started_at,
            timeout,
        }
    }

    pub(crate) async fn abort(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    #[instrument(name = "startup_prewarm.resolve", level = "trace", skip_all)]
    async fn resolve(
        self,
        cancellation_token: &CancellationToken,
    ) -> SessionStartupPrewarmResolution {
        let Self {
            mut task,
            started_at,
            timeout,
        } = self;
        let remaining = timeout.saturating_sub(started_at.elapsed());

        if task.is_finished() {
            return Self::resolution_from_join_result(task.await);
        }
        match tokio::select! {
            _ = cancellation_token.cancelled() => None,
            result = tokio::time::timeout(remaining, &mut task) => Some(result),
        } {
            Some(Ok(result)) => Self::resolution_from_join_result(result),
            Some(Err(_elapsed)) => {
                task.abort();
                info!("startup websocket prewarm timed out before the first turn could use it");
                SessionStartupPrewarmResolution::Unavailable
            }
            None => {
                task.abort();
                SessionStartupPrewarmResolution::Cancelled
            }
        }
    }

    fn resolution_from_join_result(
        result: std::result::Result<CodexResult<ModelClientSession>, tokio::task::JoinError>,
    ) -> SessionStartupPrewarmResolution {
        match result {
            Ok(Ok(prewarmed_session)) => {
                SessionStartupPrewarmResolution::Ready(Box::new(prewarmed_session))
            }
            Ok(Err(err)) => {
                warn!("startup websocket prewarm setup failed: {err:#}");
                SessionStartupPrewarmResolution::Unavailable
            }
            Err(err) => {
                warn!("startup websocket prewarm setup join failed: {err}");
                SessionStartupPrewarmResolution::Unavailable
            }
        }
    }
}

impl Session {
    pub(crate) async fn schedule_startup_prewarm(self: &Arc<Self>, input: PrewarmInput) {
        let websocket_connect_timeout = self.provider().await.websocket_connect_timeout();
        let mut state = self.state.lock().await;
        if state.shutting_down {
            return;
        }
        // Publish the warmup before another turn can be admitted. No network work
        // or awaits occur while these guards are held.
        let Ok(active_turn) = self.active_turn.try_lock() else {
            return;
        };
        if active_turn.is_some() {
            return;
        }
        if let Some(prewarm) = state.startup_prewarm.as_mut() {
            let Some(completed) = (&mut prewarm.task).now_or_never() else {
                return;
            };
            // Return a completed warmup's client to the existing cache before
            // rechecking its socket: it may have closed since the last resume.
            drop(completed);
            state.startup_prewarm = None;
        }

        if !self.services.model_client.responses_websocket_enabled() {
            return;
        }

        let started_at = Instant::now();
        let startup_prewarm = tokio::spawn(
            schedule_startup_prewarm_inner(Arc::clone(self), input).instrument(trace_span!(
                "startup_prewarm",
                thread.id = %self.thread_id(),
                prewarm.input = input.as_str(),
            )),
        );
        state.set_session_startup_prewarm(SessionStartupPrewarmHandle::new(
            startup_prewarm,
            started_at,
            websocket_connect_timeout,
        ));
    }

    pub(crate) async fn consume_startup_prewarm_for_regular_turn(
        &self,
        cancellation_token: &CancellationToken,
    ) -> SessionStartupPrewarmResolution {
        let Some(startup_prewarm) = self.take_session_startup_prewarm().await else {
            return SessionStartupPrewarmResolution::Unavailable;
        };
        startup_prewarm.resolve(cancellation_token).await
    }
}

async fn schedule_startup_prewarm_inner(
    session: Arc<Session>,
    input: PrewarmInput,
) -> CodexResult<ModelClientSession> {
    let mut client_session = session.services.model_client.new_session();
    if client_session.is_websocket_prewarmed().await {
        return Ok(client_session);
    }
    let base_instructions = session.get_prompt_base_instructions().await.text;
    let startup_turn_context = session
        .new_startup_prewarm_turn_with_sub_id(INITIAL_SUBMIT_ID.to_owned())
        .await;
    let startup_cancellation_token = CancellationToken::new();
    let preconnect_model_info = Arc::clone(startup_turn_context.model_info());
    // Spawned subagents inherit the root's selection, with the same feature and model filtering
    // that capture applies to the actual request.
    let preconnect_service_tier = if matches!(
        startup_turn_context.session_source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
    ) {
        crate::session::get_service_tier(
            session.services.agent_control.service_tier(),
            &session.features(),
            &preconnect_model_info,
        )
    } else {
        startup_turn_context.initial_settings.service_tier.clone()
    };
    // The handshake precedes tool capture; attach the finalized step metadata to the warmup.
    let (window_id, window_number, context_window_id) = session.current_window().await;
    let mut handshake_metadata = startup_turn_context
        .turn_metadata_state
        .to_responses_metadata(
            session.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Prewarm,
        );
    crate::turn_metadata::ExecutionMetadata::from_settings(&startup_turn_context.initial_settings)
        .apply_to(&mut handshake_metadata);
    let handshake_metadata = session.with_window_and_fork_metadata(
        &startup_turn_context,
        handshake_metadata,
        window_number,
        context_window_id,
    );
    // Start the handshake with the expected route while capturing tools for generate=false.
    let (step_context, ()) = tokio::try_join!(
        session.capture_step_context(
            Arc::clone(&startup_turn_context),
            &startup_cancellation_token,
        ),
        client_session.preconnect_websocket(
            &preconnect_model_info,
            preconnect_service_tier,
            &handshake_metadata,
        ),
    )?;
    let prompt_input = match input {
        PrewarmInput::Base => Vec::new(),
        PrewarmInput::History => {
            // Use the same history projection and tool metadata as a sampling request.
            // The real turn still checks this prefix before reusing the prepared response.
            let mut history = session
                .clone_history()
                .await
                .for_prompt(&step_context.settings.model_info.input_modalities);
            session
                .services
                .executed_tool_calls
                .attach_to_prompt(&mut history);
            history
        }
    };
    let startup_prompt = build_prompt(
        prompt_input,
        step_context.as_ref(),
        BaseInstructions {
            text: base_instructions,
            provenance: None,
        },
        session
            .current_window_uses_incremental_tools(&step_context)
            .await,
    );
    // Tool discovery may have updated Responses Lite metadata since the eager handshake.
    let responses_metadata = session
        .responses_metadata(step_context.as_ref(), CodexResponsesRequestKind::Prewarm)
        .await;
    // Prewarm establishes the request baseline before the first turn can change effort.
    client_session
        .prewarm_websocket(
            &startup_prompt,
            &step_context.settings.model_info,
            session
                .reasoning_effort_for_request(&step_context.settings, RequestEffortUsage::Sampling)
                .await,
            step_context.settings.reasoning_summary,
            step_context.settings.service_tier.clone(),
            &responses_metadata,
        )
        .await?;
    Ok(client_session)
}

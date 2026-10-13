use rand::Rng;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::watch;
use tokio::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

use super::process::OutputDrainPolicy;

use crate::codex_thread::BackgroundTerminalInfo;
use crate::exec::ExecRequest;
use crate::exec::ExecServerEnvConfig;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::CODEX_VERSION_ENV_VAR;
use crate::exec_env::create_env;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_session_env;
use crate::exec_env::set_tool_call_id_env_var;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventStage;
use crate::tools::runtime::ToolCtx;
use crate::tools::runtimes::unified_exec::UnifiedExecRequest as UnifiedExecToolRequest;
use crate::tools::runtimes::unified_exec::launch_unified_exec;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::MAX_YIELD_TIME_MS;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::ProcessEntry;
use crate::unified_exec::ProcessStore;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinInteractionEvent;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::async_watcher::emit_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::emit_failed_exec_end_for_unified_exec;
use crate::unified_exec::async_watcher::spawn_exit_watcher;
use crate::unified_exec::async_watcher::start_streaming_output;
use crate::unified_exec::clamp_yield_time;
use crate::unified_exec::generate_chunk_id;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::unified_exec::process::OutputBuffers;
use crate::unified_exec::process::OutputHandles;
use crate::unified_exec::process::UnifiedExecProcess;
use crate::unified_exec::shell_snapshot::shell_snapshot_request;
use crate::unified_exec::trace_id;
use codex_core_plugins::PluginCommandAttribution;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_protocol::shell_environment::is_non_inheritable_env_var;
use codex_utils_output_truncation::approx_tokens_from_byte_count;
use codex_utils_path_uri::PathUri;

const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];
const INTERRUPT: &str = "\u{3}";

pub(super) fn apply_unified_exec_env(mut env: HashMap<String, String>) -> HashMap<String, String> {
    for (key, value) in UNIFIED_EXEC_ENV {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

pub(super) fn exec_env_policy_from_shell_policy(
    policy: &ShellEnvironmentPolicy,
) -> codex_exec_server::ExecEnvPolicy {
    let mut exclude = policy
        .exclude
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    exclude.extend([
        CODEX_VERSION_ENV_VAR.to_string(),
        codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
    ]);
    let mut r#set = policy.r#set.clone();
    r#set.retain(|key, _| {
        ![
            CODEX_VERSION_ENV_VAR,
            codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
        ]
        .iter()
        .any(|runtime_key| key.eq_ignore_ascii_case(runtime_key))
            && !is_non_inheritable_env_var(key)
    });
    codex_exec_server::ExecEnvPolicy {
        inherit: policy.inherit.clone(),
        ignore_default_excludes: policy.ignore_default_excludes,
        exclude,
        r#set,
        include_only: policy
            .include_only
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

fn env_overlay_for_exec_server(
    request_env: &HashMap<String, String>,
    local_policy_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    request_env
        .iter()
        .filter(|(key, value)| {
            !is_non_inheritable_env_var(key)
                && (matches!(
                    key.as_str(),
                    CODEX_VERSION_ENV_VAR
                        | codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR
                ) || local_policy_env.get(*key) != Some(*value))
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn exec_server_env_for_request(
    request: &ExecRequest,
) -> (
    Option<codex_exec_server::ExecEnvPolicy>,
    HashMap<String, String>,
) {
    if let Some(exec_server_env_config) = &request.exec_server_env_config {
        let mut env =
            env_overlay_for_exec_server(&request.env, &exec_server_env_config.local_policy_env);
        if request.exec_server_shell_snapshot.is_some()
            && !exec_server_env_config.policy.r#set.contains_key("PATH")
        {
            env.remove("PATH");
        }
        (Some(exec_server_env_config.policy.clone()), env)
    } else {
        (None, request.env.clone())
    }
}

fn exec_server_params_for_request(
    process_id: i32,
    request: &ExecRequest,
    tool_ctx: Option<&ToolCtx>,
    tty: bool,
) -> codex_exec_server::ExecParams {
    let (env_policy, env) = exec_server_env_for_request(request);
    // Threads sharing an executor can reuse a public handle while the executor
    // still retains its previous process.
    let exec_server_process_id = format!("{process_id}-{}", Uuid::new_v4());
    codex_exec_server::ExecParams {
        process_id: exec_server_process_id.into(),
        metadata: tool_ctx.map(|ctx| codex_exec_server::ExecMetadata {
            thread_id: Some(ctx.session.thread_id()),
            tool_call_id: Some(ctx.call_id.clone()),
        }),
        argv: request.command.clone(),
        cwd: request.cwd.clone(),
        env_policy,
        shell_snapshot: request.exec_server_shell_snapshot.clone(),
        env,
        tty,
        pipe_stdin: false,
        arg0: None,
    }
}

/// Borrowed process state prepared for a `write_stdin` or poll operation.
struct PreparedProcessHandles {
    process: Arc<UnifiedExecProcess>,
    output: OutputHandles,
    pause_state: Option<watch::Receiver<bool>>,
    call_id: String,
    hook_command: String,
    process_id: i32,
    tty: bool,
}

struct InitialExecCommandGuard {
    active: Option<Arc<AtomicBool>>,
}

impl Drop for InitialExecCommandGuard {
    fn drop(&mut self) {
        if let Some(active) = self.active.as_ref() {
            active.store(false, Ordering::Release);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_failed_initial_exec_end_if_unstored(
    process_started_alive: bool,
    context: &UnifiedExecContext,
    request: &ExecCommandRequest,
    cwd: PathUri,
    plugin_attribution: Option<PluginCommandAttribution>,
    output_buffer: Arc<tokio::sync::Mutex<OutputBuffers>>,
    fallback_output: String,
    message: String,
    wall_time: Duration,
) {
    if process_started_alive {
        return;
    }

    emit_failed_exec_end_for_unified_exec(
        Arc::clone(&context.session),
        Arc::clone(&context.step_context.turn),
        Arc::clone(&context.step_context.settings.model_info),
        context.call_id.clone(),
        request.command.clone(),
        cwd,
        Some(request.process_id.to_string()),
        plugin_attribution,
        output_buffer,
        fallback_output,
        message,
        wall_time,
    )
    .await;
}

impl UnifiedExecProcessManager {
    pub(crate) async fn allocate_process_id(&self) -> i32 {
        loop {
            let mut store = self.process_store.lock().await;

            let process_id = rand::rng().random_range(1_000..100_000);

            if store.reserved_process_ids.contains(&process_id) {
                continue;
            }

            store.reserved_process_ids.insert(process_id);
            return process_id;
        }
    }

    pub(crate) async fn release_process_id(&self, process_id: i32) {
        let mut store = self.process_store.lock().await;
        store.processes.remove(&process_id);
        store.reserved_process_ids.remove(&process_id);
    }

    #[tracing::instrument(
        name = "unified_exec.exec_command",
        level = "info",
        skip_all,
        fields(
            conversation.id = %context.session.thread_id,
            turn_id = trace_id(&context.step_context.turn.sub_id),
            call_id = trace_id(&context.call_id),
            unified_exec_process_id = request.process_id,
            mode = "resumable",
            outcome = tracing::field::Empty,
        )
    )]
    pub(crate) async fn exec_command(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let result = self.exec_command_inner(request, context).await;
        let outcome = match &result {
            Ok(output) if output.process_id.is_some() => "yielded",
            Ok(_) => "exited",
            Err(_) if context.cancellation_token.is_cancelled() => "cancelled",
            Err(_) => "failed",
        };
        tracing::Span::current().record("outcome", outcome);
        result
    }

    pub(super) async fn exec_command_inner(
        &self,
        request: ExecCommandRequest,
        context: &UnifiedExecContext,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let cwd = request.cwd.clone();
        let process = match self.open_session(&request, cwd.clone(), context).await {
            Ok(process) => Arc::new(process),
            Err(err) => {
                self.release_process_id(request.process_id).await;
                return Err(err);
            }
        };
        let output_buffer = Arc::clone(&process.output_handles().output_buffer);
        let model_context = context.step_context.model_context();
        let mut event_ctx = ToolEventCtx::new(
            context.session.as_ref(),
            context.step_context.turn.as_ref(),
            &context.step_context.settings.model_info,
            &context.call_id,
            /*turn_diff_tracker*/ None,
        );
        event_ctx.model_context = Some(&model_context);
        let plugin_attribution = if request.turn_environment.environment.is_remote() {
            let file_system = request.turn_environment.environment.get_filesystem();
            context
                .step_context
                .turn
                .plugin_attribution_for_executor_command(
                    &request.command,
                    &cwd,
                    file_system.as_ref(),
                )
                .await
        } else {
            cwd.to_abs_path().ok().and_then(|cwd| {
                context
                    .step_context
                    .turn
                    .plugin_attribution_for_command(&request.command, &cwd)
            })
        };
        let emitter = ToolEmitter::unified_exec(
            &request.command,
            cwd.clone(),
            ExecCommandSource::UnifiedExecStartup,
            Some(request.process_id.to_string()),
            plugin_attribution.clone(),
        );
        emitter.emit(event_ctx, ToolEventStage::Begin).await;

        start_streaming_output(&process, context);
        let start = Instant::now();
        // Persist live sessions before the initial yield wait so interrupting the
        // turn cannot drop the last Arc and terminate the background process.
        let process_started_alive = !process.has_exited() && process.exit_code().is_none();
        let _initial_exec_command_guard = if process_started_alive {
            let initial_exec_command_active = Arc::new(AtomicBool::new(true));
            self.store_process(
                Arc::clone(&process),
                context,
                &request.command,
                request.hook_command.clone(),
                cwd.clone(),
                plugin_attribution.clone(),
                start,
                request.process_id,
                request.tty,
                Arc::clone(&output_buffer),
                Arc::clone(&initial_exec_command_active),
            )
            .await;
            InitialExecCommandGuard {
                active: Some(initial_exec_command_active),
            }
        } else {
            InitialExecCommandGuard { active: None }
        };

        let yield_time_ms = clamp_yield_time(request.yield_time_ms);
        // For the initial exec_command call, we both stream output to events
        // (via start_streaming_output above) and collect a snapshot here for
        // the tool response body.
        let deadline = start
            .checked_add(Duration::from_millis(yield_time_ms))
            .ok_or_else(|| UnifiedExecError::process_failed("yield_time_ms is too large".into()))?;
        let collected_output = Self::collect_output_until_deadline(
            process.output_handles(),
            Some(context.session.subscribe_elicitation_pause_state()),
            deadline,
        )
        .await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let text = String::from_utf8_lossy(&collected).to_string();
        let chunk_id = generate_chunk_id();
        if let Some(message) = process.failure_message() {
            emit_failed_initial_exec_end_if_unstored(
                process_started_alive,
                context,
                &request,
                cwd.clone(),
                plugin_attribution.clone(),
                Arc::clone(&output_buffer),
                text.clone(),
                message.clone(),
                wall_time,
            )
            .await;
            self.release_process_id(request.process_id).await;
            return Err(UnifiedExecError::process_failed(message));
        }
        let process_id = request.process_id;
        let (response_process_id, exit_code) = if process_started_alive {
            match self.refresh_process_state(process_id).await {
                ProcessStatus::Alive {
                    exit_code,
                    process_id,
                    ..
                } => (Some(process_id), exit_code),
                ProcessStatus::Exited { exit_code, entry } => {
                    // Collection is finished even if the remote output stream remains open.
                    entry.process.cancellation_token().cancel();
                    (None, exit_code)
                }
                ProcessStatus::Unknown => {
                    return Err(UnifiedExecError::UnknownProcessId { process_id });
                }
            }
        } else {
            // Short-lived command: emit the completed command item immediately
            // using the same helper as the background watcher.
            let exit_code = process.exit_code();
            let exit = exit_code.unwrap_or(-1);
            emit_exec_end_for_unified_exec(
                Arc::clone(&context.session),
                Arc::clone(&context.step_context.turn),
                Arc::clone(&context.step_context.settings.model_info),
                context.call_id.clone(),
                request.command.clone(),
                cwd.clone(),
                Some(process_id.to_string()),
                plugin_attribution.clone(),
                Arc::clone(&output_buffer),
                text.clone(),
                exit,
                wall_time,
            )
            .await;

            self.release_process_id(request.process_id).await;
            (None, exit_code)
        };

        let response = ExecCommandToolOutput {
            event_call_id: context.call_id.clone(),
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: context
                .step_context
                .settings
                .model_info
                .truncation_policy
                .into(),
            max_output_tokens: request.max_output_tokens,
            process_id: response_process_id,
            exit_code,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(request.hook_command.clone()),
        };

        Ok(response)
    }

    #[tracing::instrument(
        name = "unified_exec.write_stdin",
        level = "info",
        skip_all,
        fields(
            conversation.id = %context.session.thread_id,
            turn_id = trace_id(&context.step_context.turn.sub_id),
            call_id = trace_id(&context.call_id),
            original_exec_call_id = tracing::field::Empty,
            unified_exec_process_id = request.process_id,
            interaction = if request.input.is_empty() { "poll" } else { "write" },
            outcome = tracing::field::Empty,
        )
    )]
    pub(crate) async fn write_stdin(
        &self,
        context: &UnifiedExecContext,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let result = self.write_stdin_inner(request).await;
        let outcome = match &result {
            Ok(output) if output.process_id.is_some() => "yielded",
            Ok(_) => "exited",
            Err(_) if context.cancellation_token.is_cancelled() => "cancelled",
            Err(_) => "failed",
        };
        tracing::Span::current().record("outcome", outcome);
        result
    }

    async fn write_stdin_inner(
        &self,
        request: WriteStdinRequest<'_>,
    ) -> Result<ExecCommandToolOutput, UnifiedExecError> {
        let process_id = request.process_id;

        // Different terminal sessions can be polled concurrently, but reads and
        // writes against one terminal must not overlap because they share a
        // draining output buffer and process lifecycle.
        let locked_process = {
            let store = self.process_store.lock().await;
            let entry = store
                .processes
                .get(&process_id)
                .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
            // Capture the original call even if this interaction is cancelled while queued.
            if let Some(call_id) = trace_id(&entry.call_id) {
                tracing::Span::current().record("original_exec_call_id", call_id);
            }
            Arc::clone(&entry.process)
        };
        let _interaction_guard = locked_process.interaction_lock().lock_owned().await;

        // Revalidate the identity after waiting: a removed process ID can be reused.
        let PreparedProcessHandles {
            process,
            output,
            pause_state,
            call_id,
            hook_command,
            process_id,
            tty,
            ..
        } = self
            .prepare_process_handles(process_id, &locked_process)
            .await?;
        let mut status_after_write = None;

        if !request.input.is_empty() {
            if !tty {
                if request.input == INTERRUPT {
                    process.interrupt().await?;
                } else {
                    return Err(UnifiedExecError::StdinClosed);
                }
            } else {
                match process.write(request.input.as_bytes()).await {
                    Ok(()) => {
                        // Give the remote process a brief window to react so that we are
                        // more likely to capture its output in the poll below.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(err) => {
                        let status = self.refresh_process_state(process_id).await;
                        if matches!(status, ProcessStatus::Exited { .. }) {
                            status_after_write = Some(status);
                        } else if matches!(err, UnifiedExecError::ProcessFailed { .. }) {
                            process.terminate();
                            self.release_process_id(process_id).await;
                            return Err(err);
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
        }

        let yield_time_ms = {
            // Empty polls use configurable background timeout bounds. Non-empty
            // writes keep a fixed max cap so interactive stdin remains responsive.
            let time_ms = request.yield_time_ms.max(MIN_YIELD_TIME_MS);
            if request.input.is_empty() {
                time_ms.clamp(MIN_EMPTY_YIELD_TIME_MS, self.max_write_stdin_yield_time_ms)
            } else {
                time_ms.min(MAX_YIELD_TIME_MS)
            }
        };
        let start = Instant::now();
        let deadline = start + Duration::from_millis(yield_time_ms);
        let collected_output =
            Self::collect_output_until_deadline(&output, pause_state, deadline).await;
        let wall_time = Instant::now().saturating_duration_since(start);

        let original_token_count = usize::try_from(approx_tokens_from_byte_count(
            collected_output.total_bytes(),
        ))
        .unwrap_or(usize::MAX);
        let output_omitted_bytes = NonZeroUsize::new(collected_output.omitted_bytes());
        let collected = collected_output.to_bytes_with_omission_marker();
        let chunk_id = generate_chunk_id();
        if let Some(message) = process.failure_message() {
            self.release_process_id(process_id).await;
            return Err(UnifiedExecError::process_failed(message));
        }

        // After polling, refresh_process_state tells us whether the PTY is
        // still alive or has exited and been removed from the store; we thread
        // that through so the handler can tag or suppress TerminalInteraction
        // with an appropriate process_id and exit_code.
        let status = if let Some(status) = status_after_write {
            status
        } else {
            self.refresh_process_state(process_id).await
        };
        let (process_id, exit_code, event_call_id) = match status {
            ProcessStatus::Alive {
                exit_code,
                call_id,
                process_id,
            } => (Some(process_id), exit_code, call_id),
            ProcessStatus::Exited { exit_code, entry } => {
                // Release the exit watcher after the final poll, not on remote process exit.
                entry.process.cancellation_token().cancel();
                (None, exit_code, entry.call_id.clone())
            }
            ProcessStatus::Unknown => {
                if process.has_exited() {
                    (None, process.exit_code(), call_id)
                } else {
                    return Err(UnifiedExecError::UnknownProcessId {
                        process_id: request.process_id,
                    });
                }
            }
        };

        let response = ExecCommandToolOutput {
            event_call_id,
            chunk_id,
            wall_time,
            raw_output: collected,
            truncation_policy: request.truncation_policy,
            max_output_tokens: request.max_output_tokens,
            process_id,
            exit_code,
            original_token_count: Some(original_token_count),
            output_omitted_bytes,
            hook_command: Some(hook_command),
        };

        let should_emit_interaction = !request.input.is_empty() || response.process_id.is_some();
        if should_emit_interaction
            && let Some(WriteStdinInteractionEvent { session, turn }) = request.interaction_event
        {
            let interaction = TerminalInteractionEvent {
                call_id: response.event_call_id.clone(),
                process_id: response
                    .process_id
                    .unwrap_or(request.process_id)
                    .to_string(),
                stdin: request.input.to_string(),
            };
            session
                .send_event(turn.as_ref(), EventMsg::TerminalInteraction(interaction))
                .await;
        }

        Ok(response)
    }

    async fn refresh_process_state(&self, process_id: i32) -> ProcessStatus {
        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get_mut(&process_id) else {
            return ProcessStatus::Unknown;
        };

        let exit_code = entry.process.exit_code();
        let process_id = entry.process_id;

        if entry.process.has_exited() {
            let Some(entry) = store.remove(process_id) else {
                return ProcessStatus::Unknown;
            };
            ProcessStatus::Exited {
                exit_code,
                entry: Box::new(entry),
            }
        } else {
            ProcessStatus::Alive {
                exit_code,
                call_id: entry.call_id.clone(),
                process_id,
            }
        }
    }

    async fn prepare_process_handles(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> Result<PreparedProcessHandles, UnifiedExecError> {
        let mut store = self.process_store.lock().await;
        let entry = store
            .processes
            .get_mut(&process_id)
            .ok_or(UnifiedExecError::UnknownProcessId { process_id })?;
        if !Arc::ptr_eq(&entry.process, expected_process) {
            return Err(UnifiedExecError::UnknownProcessId { process_id });
        }
        entry.last_used = Instant::now();
        let output = entry.process.output_handles().clone();
        let pause_state = entry
            .session
            .upgrade()
            .map(|session| session.subscribe_elicitation_pause_state());

        Ok(PreparedProcessHandles {
            process: Arc::clone(&entry.process),
            output,
            pause_state,
            call_id: entry.call_id.clone(),
            hook_command: entry.hook_command.clone(),
            process_id: entry.process_id,
            tty: entry.tty,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_process(
        &self,
        process: Arc<UnifiedExecProcess>,
        context: &UnifiedExecContext,
        command: &[String],
        hook_command: String,
        cwd: PathUri,
        plugin_attribution: Option<PluginCommandAttribution>,
        started_at: Instant,
        process_id: i32,
        tty: bool,
        output_buffer: Arc<tokio::sync::Mutex<OutputBuffers>>,
        initial_exec_command_active: Arc<AtomicBool>,
    ) {
        let entry = ProcessEntry {
            process: Arc::clone(&process),
            call_id: context.call_id.clone(),
            process_id,
            cwd: cwd.clone(),
            initial_exec_command_active,
            hook_command,
            tty,
            session: Arc::downgrade(&context.session),
            last_used: started_at,
        };
        let pruned_entry = {
            let mut store = self.process_store.lock().await;
            let pruned_entry = Self::prune_processes_if_needed(&mut store);
            store.processes.insert(process_id, entry);
            pruned_entry
        };
        if let Some(pruned_entry) = pruned_entry {
            pruned_entry.process.terminate();
        }

        spawn_exit_watcher(
            Arc::clone(&process),
            context,
            command.to_vec(),
            cwd,
            process_id,
            plugin_attribution,
            output_buffer,
            started_at,
        );
    }

    pub(crate) async fn open_session_with_prepared_exec_env(
        &self,
        process_id: i32,
        request: &ExecRequest,
        tool_ctx: Option<&ToolCtx>,
        tty: bool,
        environment: &codex_exec_server::Environment,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        if environment.is_remote() || request.exec_server_shell_snapshot.is_some() {
            let backend = environment.get_exec_backend();
            let params = exec_server_params_for_request(process_id, request, tool_ctx, tty);
            tracing::event!(
                name: "codex.unified_exec.process_start_requested",
                target: "codex_otel.trace_safe",
                tracing::Level::INFO,
                event.name = "codex.unified_exec.process_start_requested",
                unified_exec_process_id = process_id,
                process.id = params.process_id.as_str(),
            );
            let started = backend
                .start(params)
                .await
                .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
            let output_drain_policy = if environment.is_remote() {
                OutputDrainPolicy::WaitForOutputClosure
            } else {
                OutputDrainPolicy::BoundedAfterExit
            };
            return Ok(UnifiedExecProcess::from_exec_server_started(
                started,
                output_drain_policy,
            ));
        }

        // TODO(anp): Keep PathUri through the local PTY/process launch boundary.
        let native_cwd = request
            .cwd
            .to_abs_path()
            .map_err(|_| UnifiedExecError::ForeignPath {
                path: request.cwd.clone(),
            })?;

        if request.command.is_empty() {
            return Err(UnifiedExecError::MissingCommandLine);
        }
        let spawned = codex_utils_pty::spawn_process(codex_utils_pty::SpawnRequest {
            command: &request.command,
            cwd: native_cwd.as_path(),
            env: &request.env,
            arg0: &None,
            tty,
            stdin_open: tty,
            inherited_fds: codex_utils_pty::ChildFds::Inherited(&[]),
        })
        .await
        .map_err(|err| UnifiedExecError::create_process(err.to_string()))?;
        UnifiedExecProcess::from_spawned(spawned).await
    }

    #[tracing::instrument(
        name = "unified_exec.open_session",
        level = "info",
        skip_all,
        fields(outcome = tracing::field::Empty)
    )]
    pub(super) async fn open_session(
        &self,
        request: &ExecCommandRequest,
        cwd: PathUri,
        context: &UnifiedExecContext,
    ) -> Result<UnifiedExecProcess, UnifiedExecError> {
        let shell_environment_policy = request.turn_environment.shell_environment_policy();
        let local_policy_env = create_env(shell_environment_policy, /*thread_id*/ None);
        let mut env = local_policy_env.clone();
        #[cfg(windows)]
        env.retain(|name, _| !name.eq_ignore_ascii_case(CODEX_THREAD_ID_ENV_VAR));
        env.insert(
            CODEX_THREAD_ID_ENV_VAR.to_string(),
            context.session.thread_id.to_string(),
        );
        set_tool_call_id_env_var(&mut env, Some(&context.call_id));
        inject_session_env(&mut env, context.session.session_id());
        inject_apply_patch_env(&mut env);
        let env = apply_unified_exec_env(env);
        let explicit_env_overrides = shell_environment_policy.r#set.clone();
        let exec_server_env_config = ExecServerEnvConfig {
            policy: exec_env_policy_from_shell_policy(shell_environment_policy),
            local_policy_env,
        };
        let shell_snapshot = shell_snapshot_request(request, &cwd, context);
        let req = UnifiedExecToolRequest {
            command: request.command.clone(),
            shell: request.shell.clone(),
            hook_command: request.hook_command.clone(),
            process_id: request.process_id,
            cwd,
            turn_environment: request.turn_environment.clone(),
            env,
            exec_server_env_config: Some(exec_server_env_config),
            shell_snapshot,
            explicit_env_overrides,
            tty: request.tty,
        };
        let tool_ctx = ToolCtx {
            session: context.session.clone(),
            step_context: Arc::clone(&context.step_context),
            call_id: context.call_id.clone(),
        };
        let result = launch_unified_exec(self, &req, &tool_ctx)
            .await
            .map_err(|err| UnifiedExecError::create_process(format!("{err:?}")));
        let outcome = match &result {
            Ok(_) => "completed",
            Err(_) if context.cancellation_token.is_cancelled() => "cancelled",
            Err(_) => "failed",
        };
        tracing::Span::current().record("outcome", outcome);
        result
    }

    #[tracing::instrument(
        name = "unified_exec.collect_output",
        level = "info",
        skip_all,
        fields(
            outcome = tracing::field::Empty,
            stop_reason = tracing::field::Empty,
            exit_signaled = tracing::field::Empty,
            output_closed = tracing::field::Empty,
        )
    )]
    pub(super) async fn collect_output_until_deadline<const MAX_BYTES: usize>(
        output: &OutputHandles<MAX_BYTES>,
        mut pause_state: Option<watch::Receiver<bool>>,
        mut deadline: Instant,
    ) -> HeadTailBuffer<MAX_BYTES> {
        const POST_EXIT_CLOSE_WAIT_CAP: Duration = Duration::from_millis(50);

        enum StopReason {
            OutputClosed,
            Deadline,
            PostExitDeadline,
        }

        let OutputHandles {
            output_buffer,
            output_notify,
            output_closed,
            output_closed_notify,
            cancellation_token,
        } = output;
        let mut collected = HeadTailBuffer::default();
        let mut exit_signal_received = cancellation_token.is_cancelled();
        let mut post_exit_deadline: Option<Instant> = None;
        let stop_reason = loop {
            Self::extend_deadlines_while_paused(
                &mut pause_state,
                &mut deadline,
                &mut post_exit_deadline,
            )
            .await;
            let drained_output: HeadTailBuffer<MAX_BYTES>;
            let has_drained_output: bool;
            let mut wait_for_output = None;
            {
                let mut guard = output_buffer.lock().await;
                drained_output = std::mem::take(&mut guard.pending);
                has_drained_output =
                    drained_output.retained_bytes() > 0 || drained_output.omitted_bytes() > 0;
                if !has_drained_output {
                    wait_for_output = Some(output_notify.notified());
                }
            }

            if !has_drained_output {
                exit_signal_received |= cancellation_token.is_cancelled();
                if exit_signal_received && output_closed.load(Ordering::Acquire) {
                    break StopReason::OutputClosed;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining == Duration::ZERO {
                    break StopReason::Deadline;
                }

                if exit_signal_received {
                    let now = Instant::now();
                    let close_wait_deadline = *post_exit_deadline
                        .get_or_insert_with(|| now + remaining.min(POST_EXIT_CLOSE_WAIT_CAP));
                    let close_wait_remaining = close_wait_deadline.saturating_duration_since(now);
                    if close_wait_remaining == Duration::ZERO {
                        break StopReason::PostExitDeadline;
                    }
                    let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
                    let closed = output_closed_notify.notified();
                    tokio::pin!(notified);
                    tokio::pin!(closed);
                    tokio::select! {
                        _ = &mut notified => {}
                        _ = &mut closed => {}
                        _ = tokio::time::sleep(close_wait_remaining) => break StopReason::PostExitDeadline,
                        _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                    }
                    continue;
                }

                let notified = wait_for_output.unwrap_or_else(|| output_notify.notified());
                tokio::pin!(notified);
                let exit_notified = cancellation_token.cancelled();
                tokio::pin!(exit_notified);
                tokio::select! {
                    _ = &mut notified => {}
                    _ = &mut exit_notified => exit_signal_received = true,
                    _ = tokio::time::sleep(remaining) => break StopReason::Deadline,
                    _ = Self::wait_for_pause_change(pause_state.as_ref()) => {}
                }
                continue;
            }

            collected.push_buffer(drained_output);

            exit_signal_received |= cancellation_token.is_cancelled();
            if Instant::now() >= deadline {
                break StopReason::Deadline;
            }
        };

        let span = tracing::Span::current();
        span.record(
            "stop_reason",
            match stop_reason {
                StopReason::OutputClosed => "output_closed",
                StopReason::Deadline => "deadline",
                StopReason::PostExitDeadline => "post_exit_deadline",
            },
        );
        span.record(
            "exit_signaled",
            exit_signal_received || cancellation_token.is_cancelled(),
        );
        span.record("output_closed", output_closed.load(Ordering::Acquire));
        span.record("outcome", "completed");
        collected
    }

    async fn extend_deadlines_while_paused(
        pause_state: &mut Option<watch::Receiver<bool>>,
        deadline: &mut Instant,
        post_exit_deadline: &mut Option<Instant>,
    ) {
        let Some(receiver) = pause_state.as_mut() else {
            return;
        };
        if !*receiver.borrow() {
            return;
        }

        let paused_at = Instant::now();
        while *receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }

        let paused_for = paused_at.elapsed();
        *deadline += paused_for;
        if let Some(post_exit_deadline) = post_exit_deadline.as_mut() {
            *post_exit_deadline += paused_for;
        }
    }

    async fn wait_for_pause_change(pause_state: Option<&watch::Receiver<bool>>) {
        match pause_state {
            Some(pause_state) => {
                let mut receiver = pause_state.clone();
                let _ = receiver.changed().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    fn prune_processes_if_needed(store: &mut ProcessStore) -> Option<ProcessEntry> {
        if store.processes.len() < MAX_UNIFIED_EXEC_PROCESSES {
            return None;
        }

        let mut meta: Vec<(i32, Instant, bool)> = store
            .processes
            .iter()
            .map(|(id, entry)| (*id, entry.last_used, entry.process.has_exited()))
            .collect();
        let mut found_locked_exited_process = false;

        while let Some(process_id) = Self::process_id_to_prune_from_meta(&meta) {
            let candidate_process = store
                .processes
                .get(&process_id)
                .map(|entry| Arc::clone(&entry.process));
            let candidate_has_exited = candidate_process
                .as_ref()
                .is_some_and(|process| process.has_exited());
            if found_locked_exited_process && !candidate_has_exited {
                // The store may temporarily exceed its soft cap while an exited
                // process is publishing its terminal event. Do not evict a live
                // process just because that exited process is briefly locked.
                return None;
            }

            // Do not prune processes while write_stdin or terminal event
            // publication holds their interaction lock.
            if let Some(interaction_lock) = candidate_process
                .as_ref()
                .map(|process| process.interaction_lock())
                && let Ok(_interaction_guard) = interaction_lock.try_lock_owned()
            {
                return store.remove(process_id);
            }
            found_locked_exited_process |= candidate_has_exited
                || candidate_process.is_some_and(|process| process.has_exited());
            meta.retain(|(id, _, _)| *id != process_id);
        }

        None
    }

    // Centralized pruning policy so we can easily swap strategies later.
    fn process_id_to_prune_from_meta(meta: &[(i32, Instant, bool)]) -> Option<i32> {
        if meta.is_empty() {
            return None;
        }

        let mut by_recency = meta.to_vec();
        by_recency.sort_by_key(|(_, last_used, _)| Reverse(*last_used));
        let protected: HashSet<i32> = by_recency
            .iter()
            .take(8)
            .map(|(process_id, _, _)| *process_id)
            .collect();

        let mut lru = meta.to_vec();
        lru.sort_by_key(|(_, last_used, _)| *last_used);

        if let Some((process_id, _, _)) = lru
            .iter()
            .find(|(process_id, _, exited)| !protected.contains(process_id) && *exited)
        {
            return Some(*process_id);
        }

        lru.into_iter()
            .find(|(process_id, _, _)| !protected.contains(process_id))
            .map(|(process_id, _, _)| process_id)
    }

    pub(crate) async fn terminate_all_processes(&self) {
        let entries: Vec<ProcessEntry> = {
            let mut processes = self.process_store.lock().await;
            let entries: Vec<ProcessEntry> = processes
                .processes
                .drain()
                .map(|(_, entry)| entry)
                .collect();
            processes.reserved_process_ids.clear();
            entries
        };

        for entry in entries {
            entry.process.terminate();
        }
    }

    pub(crate) async fn list_processes(&self) -> Vec<BackgroundTerminalInfo> {
        let store = self.process_store.lock().await;
        let mut entries = store
            .processes
            .values()
            .filter(|entry| !entry.process.has_exited())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.process_id);
        entries
            .into_iter()
            .map(|entry| BackgroundTerminalInfo {
                item_id: entry.call_id.clone(),
                process_id: entry.process_id.to_string(),
                command: entry.hook_command.clone(),
                cwd: entry.cwd.clone(),
            })
            .collect()
    }

    pub(crate) async fn terminate_process(&self, process_id: i32) -> bool {
        let (process, already_exited) = {
            let store = self.process_store.lock().await;
            let Some(entry) = store.processes.get(&process_id) else {
                return false;
            };
            (Arc::clone(&entry.process), entry.process.has_exited())
        };

        if !already_exited && process.terminate_confirmed().await.is_err() {
            return false;
        }
        process.cancellation_token().cancel();

        let mut store = self.process_store.lock().await;
        let Some(entry) = store.processes.get(&process_id) else {
            return true;
        };
        if !Arc::ptr_eq(&entry.process, &process)
            || entry.initial_exec_command_active.load(Ordering::Acquire)
        {
            return true;
        }
        store.remove(process_id).is_some()
    }
}

enum ProcessStatus {
    Alive {
        exit_code: Option<i32>,
        call_id: String,
        process_id: i32,
    },
    Exited {
        exit_code: Option<i32>,
        entry: Box<ProcessEntry>,
    },
    Unknown,
}

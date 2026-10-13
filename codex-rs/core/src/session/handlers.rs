use super::Submission;
use crate::WithTurnExtensionData;
use async_channel::Receiver;
use codex_otel::set_parent_from_w3c_trace_context;
use tracing::Instrument;
use tracing::info_span;

use crate::session::session::Session;
use crate::session::thread_settings;
use crate::session::turn_input;

use crate::config::Config;
use crate::review_prompts::resolve_review_request;
use crate::session::spawn_review_thread;
use crate::tasks::CompactTask;
use crate::tasks::UserShellCommandMode;
use crate::tasks::UserShellCommandTask;
use crate::tasks::execute_user_shell_command;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::request_user_input::RequestUserInputResponse;

use crate::context_manager::is_user_turn_boundary;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::mcp::RequestId as ProtocolRequestId;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use serde_json::Value;
use std::sync::Arc;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub async fn interrupt(sess: &Arc<Session>) {
    sess.interrupt_task().await;
}

pub async fn clean_background_terminals(sess: &Arc<Session>) {
    sess.close_unified_exec_processes().await;
}

/// Queues an inter-agent message, then lets the shared pending-work scheduler
/// decide whether an idle session should start a regular turn.
pub async fn inter_agent_communication(
    sess: &Arc<Session>,
    sub_id: String,
    communication: InterAgentCommunication,
    start_options: codex_protocol::turn_input::TurnStartOptions,
) {
    let trigger_turn = communication.trigger_turn;
    sess.input_queue
        .enqueue_mailbox_communication(communication, start_options)
        .await;
    crate::agent_communication::emit_agent_communication_receive(&sub_id);
    if trigger_turn || sess.has_outstanding_durable_sleep() {
        sess.maybe_start_turn_for_pending_work_with_sub_id(sub_id)
            .await;
    }
}

pub async fn run_user_shell_command(
    sess: &Arc<Session>,
    sub_id: String,
    command: String,
    timeout_ms: Option<u64>,
) {
    if let Some((turn_context, cancellation_token)) =
        sess.active_turn_context_and_cancellation_token().await
    {
        let session = Arc::clone(sess);
        tokio::spawn(async move {
            execute_user_shell_command(
                session,
                turn_context,
                command,
                timeout_ms,
                cancellation_token,
                UserShellCommandMode::ActiveTurnAuxiliary,
            )
            .await;
        });
        return;
    }

    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;
    sess.spawn_task(
        turn_context,
        Vec::new(),
        UserShellCommandTask::new(command, timeout_ms),
    )
    .await;
}

pub async fn resolve_elicitation(
    sess: &Arc<Session>,
    server_name: String,
    request_id: ProtocolRequestId,
    decision: codex_protocol::elicitation::ElicitationAction,
    content: Option<Value>,
    meta: Option<Value>,
) {
    let action = match decision {
        codex_protocol::elicitation::ElicitationAction::Accept => ElicitationAction::Accept,
        codex_protocol::elicitation::ElicitationAction::Decline => ElicitationAction::Decline,
        codex_protocol::elicitation::ElicitationAction::Cancel => ElicitationAction::Cancel,
    };
    let content = match action {
        // Preserve the legacy fallback for clients that only send an action.
        ElicitationAction::Accept => Some(content.unwrap_or_else(|| serde_json::json!({}))),
        ElicitationAction::Decline | ElicitationAction::Cancel => None,
        _ => None,
    };
    let response = ElicitationResponse {
        action,
        content,
        meta,
    };
    let request_id = match request_id {
        ProtocolRequestId::String(value) => {
            rmcp::model::NumberOrString::String(std::sync::Arc::from(value))
        }
        ProtocolRequestId::Integer(value) => rmcp::model::NumberOrString::Number(value),
    };
    if let Err(err) = sess
        .resolve_elicitation(server_name, request_id, response)
        .await
    {
        warn!(
            error = %err,
            "failed to resolve elicitation request in session"
        );
    }
}

pub async fn request_user_input_response(
    sess: &Arc<Session>,
    id: String,
    response: RequestUserInputResponse,
) {
    sess.notify_user_input_response(&id, response).await;
}

pub async fn dynamic_tool_response(sess: &Arc<Session>, id: String, response: DynamicToolResponse) {
    sess.notify_dynamic_tool_response(&id, response).await;
}

pub fn refresh_mcp_servers(sess: &Session) {
    sess.services.mcp_runtime.reconnect_on_next_refresh();
    sess.request_mcp_runtime_refresh();
}

pub async fn reload_user_config(sess: &Arc<Session>) {
    Box::pin(sess.reload_user_config_layer()).await;
}

pub async fn compact(sess: &Arc<Session>, sub_id: String) {
    // Stop the old turn before the compact task picks up the next turn's environments.
    sess.abort_all_tasks(TurnAbortReason::Replaced).await;
    let turn_context = sess
        .new_turn_with_default_settings(sub_id, Default::default())
        .await;

    sess.spawn_task(turn_context, Vec::new(), CompactTask).await;
}

pub(super) async fn shutdown_session_runtime(sess: &Arc<Session>) {
    let startup_prewarm = {
        let mut state = sess.state.lock().await;
        // Stop admission and take the current warmup together so resume cannot replace it.
        state.shutting_down = true;
        state.take_session_startup_prewarm()
    };
    if let Some(startup_prewarm) = startup_prewarm {
        startup_prewarm.abort().await;
    }
    sess.abort_all_tasks(TurnAbortReason::Interrupted).await;
    let shell_snapshot_prewarm = sess.state.lock().await.shell_snapshot_prewarm.take();
    if let Some(shell_snapshot_prewarm) = shell_snapshot_prewarm {
        shell_snapshot_prewarm.abort();
        let _ = shell_snapshot_prewarm.await;
    }
    sess.hooks().shutdown().await;
    sess.async_hook_results.close();
    while sess.async_hook_results.try_recv().is_ok() {}
    sess.services
        .unified_exec_manager
        .terminate_all_processes()
        .await;
    sess.stop_mcp_prewarm_worker().await;
    {
        let _refresh = sess.mcp_refresh.acquire().await;
        sess.mcp_refresh.close();
        sess.services.mcp_runtime.shutdown().await;
    }

    crate::hook_runtime::run_session_end_hooks(sess).await;
    emit_thread_stop_lifecycle(sess).await;
}

async fn emit_thread_stop_lifecycle(sess: &Session) {
    for contributor in sess.services.extensions.thread_lifecycle_contributors() {
        contributor
            .on_thread_stop(codex_extension_api::ThreadStopInput {
                session_store: &sess.services.session_extension_data,
                thread_store: &sess.services.thread_extension_data,
            })
            .await;
    }
}

pub async fn shutdown(sess: &Arc<Session>, sub_id: String) -> bool {
    shutdown_session_runtime(sess).await;
    info!("Shutting down Codex instance");
    let history = sess.clone_history().await;
    let turn_count = history
        .raw_items()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    sess.services.session_telemetry.counter(
        "codex.conversation.turn.count",
        i64::try_from(turn_count).unwrap_or(0),
        &[],
    );

    let event = Event {
        id: sub_id,
        msg: EventMsg::ShutdownComplete,
    };
    sess.deliver_event_raw(event).await;
    true
}

pub async fn review(
    sess: &Arc<Session>,
    config: &Arc<Config>,
    sub_id: String,
    review_request: ReviewRequest,
) {
    let turn_context = sess
        .new_turn_with_default_settings(sub_id.clone(), Default::default())
        .await;
    sess.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;
    #[allow(deprecated)]
    match resolve_review_request(review_request, &turn_context.cwd) {
        Ok(resolved) => {
            spawn_review_thread(
                Arc::clone(sess),
                Arc::clone(config),
                turn_context.clone(),
                sub_id,
                resolved,
            )
            .await;
        }
        Err(err) => {
            let event = Event {
                id: sub_id,
                msg: EventMsg::Error(ErrorEvent {
                    misalignment: None,
                    message: err.to_string(),
                    codex_error_info: Some(CodexErrorInfo::Other),
                }),
            };
            sess.send_event(&turn_context, event.msg).await;
        }
    }
}

pub(super) async fn submission_loop(
    sess: Arc<Session>,
    config: Arc<Config>,
    rx_sub: Receiver<Submission>,
) {
    // Session shutdown and tree shutdown both use the existing teardown handler.
    let mut shutdown_received = false;
    let mut mailbox = sess.input_queue.mailbox_updates();
    if let Some(updates) = &mut mailbox {
        updates.mark_changed();
    }
    loop {
        let sub = tokio::select! {
            biased;
            _ = sess.services.local_agent_runtime.shutdown.cancelled() => {
                shutdown_received = shutdown(&sess, super::new_submission_id()).await;
                break;
            }
            sub = rx_sub.recv() => match sub {
                Ok(sub) => sub,
                Err(_) => break,
            },
            update = async {
                match &mut mailbox {
                    Some(updates) => updates.changed().await.map(|()| *updates.borrow_and_update()),
                    None => std::future::pending().await,
                }
            } => {
                match update {
                    Ok(true) => {
                        sess.input_queue.notify_mailbox();
                        if sess.has_outstanding_durable_sleep() {
                            sess.maybe_start_turn_for_pending_work().await;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => mailbox = None,
                }
                continue;
            }
        };
        debug!(
            thread_id = %sess.thread_id(),
            submission_id = %sub.id,
            operation = sub.op.kind(),
            parent_turn_id = ?sub.parent_turn_id,
            root_turn_id = ?sub.root_turn_id,
            op = ?sub.op,
            "Submission"
        );
        let dispatch_span = submission_dispatch_span(&sub);
        let should_exit = async {
            match sub.op {
                Op::Interrupt => {
                    interrupt(&sess).await;
                    false
                }
                Op::InterruptIfNoPendingInput { turn_id, reply } => {
                    sess.interrupt_turn_if_no_pending_input(&turn_id, reply)
                        .await;
                    false
                }
                Op::CleanBackgroundTerminals => {
                    clean_background_terminals(&sess).await;
                    false
                }
                Op::TurnInput {
                    request,
                    mode,
                    reply,
                } => {
                    let request = WithTurnExtensionData {
                        request: *request,
                        turn_extension_init: sub.turn_extension_init,
                    };
                    let result = turn_input::handle(&sess, request, mode, sub.id.clone()).await;
                    let _ = reply.send(result);
                    false
                }
                Op::ThreadSettings {
                    thread_settings,
                    reply,
                } => {
                    let _settings_guard = thread_settings::acquire_persistence_lock(&sess).await;
                    let thread_settings = WithTurnExtensionData {
                        request: thread_settings,
                        turn_extension_init: sub.turn_extension_init,
                    };
                    match thread_settings::update(&sess, thread_settings).await {
                        Ok(snapshot) => {
                            // Reply first: the caller may hold a lock its event consumer needs.
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                            thread_settings::emit_applied(&sess, sub.id.clone(), snapshot).await;
                        }
                        Err(error) => {
                            let message = format!("invalid thread settings override: {error}");
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(CodexErr::InvalidRequest(message)));
                            } else {
                                sess.send_event_raw(Event {
                                    id: sub.id.clone(),
                                    msg: EventMsg::Error(ErrorEvent {
                                        misalignment: None,
                                        message,
                                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                                    }),
                                })
                                .await;
                            }
                        }
                    }
                    false
                }
                Op::TurnSettings {
                    turn_id,
                    update,
                    reply,
                } => {
                    let outcome = sess.apply_turn_settings(&turn_id, update).await;
                    let _ = reply.send(outcome);
                    false
                }
                Op::InterAgentCommunication {
                    communication,
                    start_options,
                } => {
                    inter_agent_communication(&sess, sub.id.clone(), communication, start_options)
                        .await;
                    false
                }
                Op::UserInputAnswer { id, response } => {
                    request_user_input_response(&sess, id, response).await;
                    false
                }
                Op::DynamicToolResponse { id, response } => {
                    dynamic_tool_response(&sess, id, response).await;
                    false
                }
                Op::RefreshMcpServers => {
                    refresh_mcp_servers(&sess);
                    false
                }
                Op::ReloadUserConfig => {
                    reload_user_config(&sess).await;
                    false
                }
                Op::Compact => {
                    compact(&sess, sub.id.clone()).await;
                    false
                }
                Op::RunUserShellCommand {
                    command,
                    timeout_ms,
                } => {
                    run_user_shell_command(&sess, sub.id.clone(), command, timeout_ms).await;
                    false
                }
                Op::ResolveElicitation {
                    server_name,
                    request_id,
                    decision,
                    content,
                    meta,
                } => {
                    resolve_elicitation(&sess, server_name, request_id, decision, content, meta)
                        .await;
                    false
                }
                Op::Shutdown => shutdown(&sess, sub.id.clone()).await,
                Op::Review { review_request } => {
                    review(&sess, &config, sub.id.clone(), review_request).await;
                    false
                }
                _ => false, // Ignore unknown ops; enum is non_exhaustive to allow extensions.
            }
        }
        .instrument(dispatch_span)
        .await;
        drop(sub.residency_guard);
        if should_exit {
            shutdown_received = true;
            break;
        }
    }
    // If the submission loop exits because the channel closed without an
    // explicit shutdown op, still run session teardown.
    if !shutdown_received {
        shutdown_session_runtime(&sess).await;
    }
    debug!("Agent loop exited");
}

pub(super) fn submission_dispatch_span(sub: &Submission) -> tracing::Span {
    let op_name = sub.op.kind();
    let span_name = format!("op.dispatch.{op_name}");
    let dispatch_span = info_span!(
        "submission_dispatch",
        otel.name = span_name.as_str(),
        submission.id = sub.id.as_str(),
        codex.op = op_name
    );
    if let Some(trace) = sub.trace.as_ref()
        && !set_parent_from_w3c_trace_context(&dispatch_span, trace)
    {
        warn!(
            submission.id = sub.id.as_str(),
            "ignoring invalid submission trace carrier"
        );
    }
    dispatch_span
}

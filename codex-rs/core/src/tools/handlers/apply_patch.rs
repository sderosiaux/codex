use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::apply_patch::convert_apply_patch_to_protocol;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::context::ApplyPatchToolOutput;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::handlers::apply_patch_spec::create_apply_patch_freeform_tool;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolExecutor;
use crate::tools::runtime::ToolCtx;
use crate::tools::runtimes::apply_patch::ApplyPatchRequest;
use crate::tools::runtimes::apply_patch::ApplyPatchRuntime;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::Hunk;
use codex_apply_patch::StreamingPatchParser;
use codex_exec_server::ExecutorFileSystem;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyUpdatedEvent;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_path_uri::PathUri;

const APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL: Duration = Duration::from_millis(500);

/// Handles freeform `apply_patch` requests and routes verified patches to the
/// selected environment filesystem.
#[derive(Default)]
pub struct ApplyPatchHandler;

#[derive(Default)]
struct ApplyPatchArgumentDiffConsumer {
    parser: StreamingPatchParser,
    last_sent_at: Option<Instant>,
    pending: Option<PatchApplyUpdatedEvent>,
}

impl ToolArgumentDiffConsumer for ApplyPatchArgumentDiffConsumer {
    fn consume_diff(
        &mut self,
        turn: &TurnContext,
        call_id: String,
        diff: &str,
    ) -> Option<EventMsg> {
        if !turn
            .config
            .features
            .enabled(Feature::ApplyPatchStreamingEvents)
        {
            return None;
        }

        self.push_delta(call_id, diff)
            .map(EventMsg::PatchApplyUpdated)
    }

    fn finish(&mut self) -> Result<Option<EventMsg>, FunctionCallError> {
        self.finish_update_on_complete()
            .map(|event| event.map(EventMsg::PatchApplyUpdated))
    }
}

impl ApplyPatchArgumentDiffConsumer {
    fn push_delta(&mut self, call_id: String, delta: &str) -> Option<PatchApplyUpdatedEvent> {
        let hunks = self.parser.push_delta(delta).ok()?;
        if hunks.is_empty() {
            return None;
        }
        let changes = convert_apply_patch_hunks_to_protocol(&hunks);
        let event = PatchApplyUpdatedEvent { call_id, changes };
        let now = Instant::now();
        match self.last_sent_at {
            Some(last_sent_at)
                if now.duration_since(last_sent_at) < APPLY_PATCH_ARGUMENT_DIFF_BUFFER_INTERVAL =>
            {
                self.pending = Some(event);
                None
            }
            Some(_) | None => {
                self.pending = None;
                self.last_sent_at = Some(now);
                Some(event)
            }
        }
    }

    fn finish_update_on_complete(
        &mut self,
    ) -> Result<Option<PatchApplyUpdatedEvent>, FunctionCallError> {
        self.parser.finish().map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse apply_patch: {err}"))
        })?;

        let event = self.pending.take();
        if event.is_some() {
            self.last_sent_at = Some(Instant::now());
        }
        Ok(event)
    }
}

fn convert_apply_patch_hunks_to_protocol(hunks: &[Hunk]) -> HashMap<PathBuf, FileChange> {
    hunks
        .iter()
        .map(|hunk| {
            let path = hunk_source_path(hunk).to_path_buf();
            let change = match hunk {
                Hunk::AddFile { contents, .. } => FileChange::Add {
                    content: contents.clone(),
                },
                Hunk::DeleteFile { .. } => FileChange::Delete {
                    content: String::new(),
                },
                Hunk::UpdateFile {
                    chunks, move_path, ..
                } => FileChange::Update {
                    unified_diff: format_update_chunks_for_progress(chunks),
                    move_path: move_path.clone(),
                },
            };
            (path, change)
        })
        .collect()
}

fn hunk_source_path(hunk: &Hunk) -> &Path {
    match hunk {
        Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } | Hunk::UpdateFile { path, .. } => {
            path
        }
    }
}

fn format_update_chunks_for_progress(chunks: &[codex_apply_patch::UpdateFileChunk]) -> String {
    let mut unified_diff = String::new();
    for chunk in chunks {
        match &chunk.change_context {
            Some(context) => {
                unified_diff.push_str("@@ ");
                unified_diff.push_str(context);
                unified_diff.push('\n');
            }
            None => {
                unified_diff.push_str("@@");
                unified_diff.push('\n');
            }
        }
        for line in &chunk.old_lines {
            unified_diff.push('-');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        for line in &chunk.new_lines {
            unified_diff.push('+');
            unified_diff.push_str(line);
            unified_diff.push('\n');
        }
        if chunk.is_end_of_file {
            unified_diff.push_str("*** End of File");
            unified_diff.push('\n');
        }
    }
    unified_diff
}

/// Extracts the raw patch text used as the command-shaped hook input for apply_patch.
fn apply_patch_payload_command(payload: &ToolPayload) -> Option<String> {
    match payload {
        ToolPayload::Custom { input } => Some(input.clone()),
        _ => None,
    }
}

impl ToolExecutor<ToolInvocation> for ApplyPatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("apply_patch")
    }

    fn spec(&self) -> ToolSpec {
        create_apply_patch_freeform_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ApplyPatchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let ToolPayload::Custom { input: patch_input } = payload else {
            return Err(FunctionCallError::RespondToModel(
                "apply_patch handler received unsupported payload".to_string(),
            ));
        };
        let args = match codex_apply_patch::parse_patch(&patch_input) {
            Ok(args) => args,
            Err(parse_error) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )));
            }
        };
        // Verify the parsed patch against the selected environment filesystem.
        let turn_environment =
            resolve_tool_environment(&step_context, "apply_patch is unavailable in this session")?;
        let fs = turn_environment.environment.get_filesystem();
        match codex_apply_patch::verify_apply_patch_args(args, turn_environment.cwd(), fs.as_ref())
            .await
        {
            codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
                let tool_ctx = ToolCtx {
                    session,
                    step_context: Arc::clone(&step_context),
                    call_id,
                };
                let content = execute_verified_patch(
                    changes,
                    turn_environment.clone(),
                    Some(&tracker),
                    tool_ctx,
                )
                .await?;
                Ok(boxed_tool_output(ApplyPatchToolOutput::from_text(content)))
            }
            codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
                Err(FunctionCallError::RespondToModel(format!(
                    "apply_patch verification failed: {parse_error}"
                )))
            }
            codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
                tracing::trace!("Failed to parse apply_patch input, {error:?}");
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received invalid patch input".to_string(),
                ))
            }
            codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => {
                Err(FunctionCallError::RespondToModel(
                    "apply_patch handler received non-apply_patch input".to_string(),
                ))
            }
        }
    }
}

impl CoreToolRuntime for ApplyPatchHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }

    fn create_diff_consumer(&self) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        Some(Box::<ApplyPatchArgumentDiffConsumer>::default())
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        apply_patch_payload_command(&invocation.payload).map(|command| PreToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_input: serde_json::json!({ "command": command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let patch = updated_hook_command(&updated_input)?;
        invocation.payload = match invocation.payload {
            ToolPayload::Custom { .. } => ToolPayload::Custom {
                input: patch.to_string(),
            },
            payload => payload,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let tool_response =
            result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::apply_patch(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: serde_json::json!({
                "command": apply_patch_payload_command(&invocation.payload)?,
            }),
            tool_response,
        })
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn intercept_apply_patch(
    command: &[String],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    turn_environment: TurnEnvironment,
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tracker: Option<&SharedTurnDiffTracker>,
    call_id: &str,
) -> Result<Option<FunctionToolOutput>, FunctionCallError> {
    match codex_apply_patch::maybe_parse_apply_patch_verified(command, cwd, fs).await {
        codex_apply_patch::MaybeApplyPatchVerified::Body(changes) => {
            let tool_ctx = ToolCtx {
                session,
                step_context,
                call_id: call_id.to_string(),
            };
            let content =
                execute_verified_patch(changes, turn_environment, tracker, tool_ctx).await?;
            Ok(Some(FunctionToolOutput::from_text(content, Some(true))))
        }
        codex_apply_patch::MaybeApplyPatchVerified::CorrectnessError(parse_error) => {
            Err(FunctionCallError::RespondToModel(format!(
                "apply_patch verification failed: {parse_error}"
            )))
        }
        codex_apply_patch::MaybeApplyPatchVerified::ShellParseError(error) => {
            tracing::trace!("Failed to parse apply_patch input, {error:?}");
            Ok(None)
        }
        codex_apply_patch::MaybeApplyPatchVerified::NotApplyPatch => Ok(None),
    }
}

async fn execute_verified_patch(
    action: ApplyPatchAction,
    turn_environment: TurnEnvironment,
    tracker: Option<&SharedTurnDiffTracker>,
    tool_ctx: ToolCtx,
) -> Result<String, FunctionCallError> {
    let changes = convert_apply_patch_to_protocol(&action);
    let emitter = ToolEmitter::apply_patch_for_environment(
        changes,
        turn_environment.selection.environment_id.clone(),
    );
    let event_ctx = ToolEventCtx::new(
        tool_ctx.session.as_ref(),
        tool_ctx.step_context.turn.as_ref(),
        &tool_ctx.step_context.settings.model_info,
        &tool_ctx.call_id,
        tracker,
    );
    emitter.begin(event_ctx).await;

    let request = ApplyPatchRequest {
        turn_environment,
        action,
    };
    let mut runtime = ApplyPatchRuntime::new();
    let result = runtime.run(&request).await;
    let (result, delta) = match result {
        Ok(output) => (Ok(output.exec_output), Some(output.delta)),
        Err(error) => (Err(error), Some(runtime.committed_delta().clone())),
    };
    let event_ctx = ToolEventCtx::new(
        tool_ctx.session.as_ref(),
        tool_ctx.step_context.turn.as_ref(),
        &tool_ctx.step_context.settings.model_info,
        &tool_ctx.call_id,
        tracker,
    );
    emitter.finish(event_ctx, result, delta.as_ref()).await
}

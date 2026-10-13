use crate::context::UserGoalUpdate;
use codex_protocol::ResponseItemId;
use std::collections::HashSet;
use std::sync::Arc;

use super::RemoteCompactionV2Output;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact_remote_history::trim_function_call_history_to_fit_context_window;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_history::CodexHarnessMetadata;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use tracing::info;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) input_goal_ids: HashSet<ResponseItemId>,
    pub(super) prompt_input: Vec<ResponseItem>,
    pub(super) prompt_input_metadata: Vec<Option<CodexHarnessMetadata>>,
    pub(super) compaction_output: ResponseItem,
    pub(super) compaction_response_id: String,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let input_goal_ids = UserGoalUpdate::message_ids(history.raw_items());
    let tools_in_history = step_context.settings.model_info.use_responses_lite
        && history
            .raw_items()
            .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }));
    let base_instructions = if tools_in_history {
        BaseInstructions {
            text: String::new(),
            provenance: None,
        }
    } else {
        sess.get_prompt_base_instructions().await
    };
    let rewritten_outputs = trim_function_call_history_to_fit_context_window(
        &mut history,
        turn_context.as_ref(),
        &base_instructions,
    );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction v2"
        );
    }

    let (mut input, prompt_input_metadata): (Vec<_>, Vec<_>) = history
        .for_prompt_annotated(&turn_context.model_info().input_modalities)
        .into_iter()
        .map(|envelope| (envelope.item, envelope.metadata))
        .unzip();
    sess.services
        .executed_tool_calls
        .attach_to_compaction_prompt(&mut input);
    let tool_router = &step_context.tool_router;
    input.push(ResponseItem::CompactionTrigger {});
    let prompt = Prompt {
        input,
        tools: if tools_in_history {
            Arc::default()
        } else {
            tool_router.model_visible_specs()
        },
        parallel_tool_calls: true,
        base_instructions,
        output_schema: None,
        output_schema_strict: true,
        cyber_access_program: turn_context.cyber_access_program,
    };

    let mut responses_metadata = sess
        .compaction_responses_metadata(turn_context.as_ref(), compaction_metadata)
        .await;
    responses_metadata.tool_namespaces_info = tool_router.tool_namespaces_info().cloned();
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage,
    } = run_remote_compaction_request_v2(
        sess,
        step_context,
        client_session,
        &prompt,
        &responses_metadata,
    )
    .await?;
    let mut prompt_input = prompt.input;
    prompt_input.pop();
    Ok(RemoteCompactV2Attempt {
        input_goal_ids,
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        compaction_response_id: response_id,
        token_usage,
        owned_client_session,
    })
}

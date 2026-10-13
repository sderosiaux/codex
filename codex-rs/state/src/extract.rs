use crate::model::ThreadMetadata;
use codex_history::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::protocol::user_message_preview;
use serde::Serialize;
use serde_json::Value;

/// Apply a rollout item to the metadata structure.
pub fn apply_rollout_item(
    metadata: &mut ThreadMetadata,
    item: &RolloutItem,
    default_provider: &str,
) {
    match item {
        RolloutItem::SessionMeta(meta_line) => apply_session_meta_from_item(metadata, meta_line),
        RolloutItem::TurnContext(turn_ctx) => apply_turn_context(metadata, turn_ctx),
        RolloutItem::EventMsg(event) => apply_event_msg(metadata, event),
        RolloutItem::ResponseItem(item) => apply_response_item(metadata, &item.item),
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. } => {}
        RolloutItem::Compacted(_) => {}
        RolloutItem::WorldState(_) => {}
        RolloutItem::RetainedContext(_) => {}
        RolloutItem::TokenUsageRecord(_) => {}
    }
    if metadata.model_provider.is_empty() {
        metadata.model_provider = default_provider.to_string();
    }
}

/// Return whether this rollout item can mutate thread metadata stored in SQLite.
pub fn rollout_item_affects_thread_metadata(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::SessionMeta(_) | RolloutItem::TurnContext(_) => true,
        RolloutItem::EventMsg(
            EventMsg::TokenCount(_)
            | EventMsg::UserMessage(_)
            | EventMsg::ThreadGoalUpdated(_)
            | EventMsg::ThreadSettingsApplied(_),
        ) => true,
        RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
            if matches!(
                event.item,
                TurnItem::UserMessage(_) | TurnItem::FunctionCallOutput(_)
            ) =>
        {
            true
        }
        RolloutItem::EventMsg(_)
        | RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::RetainedContext(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::WorldState(_) => false,
    }
}

fn apply_session_meta_from_item(metadata: &mut ThreadMetadata, meta_line: &SessionMetaLine) {
    if metadata.id != meta_line.meta.id {
        // Ignore session_meta lines that don't match the canonical thread ID,
        // e.g., forked rollouts that embed the source session metadata.
        return;
    }
    metadata.creator_user_id = metadata
        .creator_user_id
        .take()
        .or_else(|| meta_line.meta.creator_user_id.clone());
    metadata.creator_account_id = metadata
        .creator_account_id
        .take()
        .or_else(|| meta_line.meta.creator_account_id.clone());
    metadata.id = meta_line.meta.id;
    metadata.source = enum_to_string(&meta_line.meta.source);
    if metadata.originator.is_none() && !meta_line.meta.originator.is_empty() {
        metadata.originator = Some(meta_line.meta.originator.clone());
    }
    // Later SessionMeta lines do not redefine the canonical history_mode.
    metadata.thread_source = meta_line.meta.thread_source.clone();
    metadata.agent_nickname = meta_line.meta.agent_nickname.clone();
    metadata.agent_role = meta_line.meta.agent_role.clone();
    metadata.agent_path = meta_line.meta.agent_path.clone();
    if let Some(provider) = meta_line.meta.model_provider.as_deref() {
        metadata.model_provider = provider.to_string();
    }
    if !meta_line.meta.cli_version.is_empty() {
        metadata.cli_version = meta_line.meta.cli_version.clone();
    }
    if !meta_line.meta.cwd.as_os_str().is_empty() {
        metadata.cwd = meta_line.meta.cwd.clone();
    }
    if let Some(git) = meta_line.git.as_ref() {
        metadata.git_sha = git.commit_hash.as_ref().map(|sha| sha.0.clone());
        metadata.git_branch = git.branch.clone();
        metadata.git_origin_url = git.repository_url.clone();
    }
}

fn apply_turn_context(metadata: &mut ThreadMetadata, turn_ctx: &TurnContextItem) {
    if metadata.cwd.as_os_str().is_empty() {
        metadata.cwd = turn_ctx.cwd.clone().into_path_buf();
    }
    metadata.model = Some(turn_ctx.model.clone());
    metadata.reasoning_effort = turn_ctx.effort.clone();
}

fn apply_event_msg(metadata: &mut ThreadMetadata, event: &EventMsg) {
    match event {
        EventMsg::TokenCount(token_count) => {
            if let Some(info) = token_count.info.as_ref() {
                metadata.tokens_used = info.total_token_usage.total_tokens.max(0);
            }
        }
        EventMsg::UserMessage(user) => {
            apply_user_message(metadata, user);
        }
        EventMsg::ItemCompleted(event) => {
            if let TurnItem::UserMessage(user) = &event.item {
                apply_user_message(metadata, &user.as_legacy_user_message_event());
            } else if let TurnItem::FunctionCallOutput(output) = &event.item {
                // Delegated turns have no user message, but must still be discoverable.
                let preview = crate::delegated_output_preview(output);
                set_preview_if_empty(metadata, preview);
            }
        }
        EventMsg::ThreadGoalUpdated(event) => {
            let objective = event.goal.objective.trim();
            if !objective.is_empty() {
                set_preview_if_empty(metadata, Some(objective.to_string()));
            }
        }
        EventMsg::ThreadSettingsApplied(event) => {
            let settings = &event.thread_settings;
            metadata.model = Some(settings.model.clone());
            metadata.model_provider = settings.model_provider_id.clone();
            metadata.reasoning_effort = settings.reasoning_effort.clone();
            metadata.cwd = settings.cwd.clone().into_path_buf();
        }
        _ => {}
    }
}

fn apply_response_item(_metadata: &mut ThreadMetadata, _item: &ResponseItem) {}

fn apply_user_message(metadata: &mut ThreadMetadata, user: &UserMessageEvent) {
    let preview = user_message_preview(user);
    if metadata.first_user_message.is_none() {
        metadata.first_user_message = preview.clone();
    }
    set_preview_if_empty(metadata, preview);
    if metadata.title.is_empty() {
        let title = strip_user_message_prefix(user.message.as_str());
        if !title.is_empty() {
            metadata.title = title.to_string();
        }
    }
}

fn set_preview_if_empty(metadata: &mut ThreadMetadata, preview: Option<String>) {
    if metadata.preview.is_none() {
        metadata.preview = preview;
    }
}

pub(crate) fn enum_to_string<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

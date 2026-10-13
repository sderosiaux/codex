use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;

use crate::engine::ConfiguredHandler;
use crate::engine::HandlerSourcePath;
use crate::engine::dispatcher;
use crate::output_spill::AdditionalContext;

/// Identifies a thread-spawned subagent when a normal hook runs inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentHookContext {
    pub agent_id: String,
    pub agent_type: String,
}

pub(crate) fn join_text_chunks(chunks: Vec<String>) -> Option<String> {
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n\n"))
    }
}

pub(crate) fn trimmed_non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn append_additional_context(
    entries: &mut Vec<HookOutputEntry>,
    additional_contexts_for_model: &mut Vec<AdditionalContext>,
    handler: &ConfiguredHandler,
    additional_context: String,
) {
    entries.push(HookOutputEntry {
        kind: HookOutputEntryKind::Context,
        text: additional_context.clone(),
    });
    additional_contexts_for_model.push(AdditionalContext {
        text: additional_context,
        limit: handler.additional_context_limit,
    });
}

pub(crate) fn flatten_additional_contexts<'a>(
    additional_contexts: impl IntoIterator<Item = &'a [AdditionalContext]>,
) -> Vec<AdditionalContext> {
    additional_contexts
        .into_iter()
        .flat_map(|chunk| chunk.iter().cloned())
        .collect()
}

pub(crate) fn serialization_failure_hook_events(
    handlers: Vec<ConfiguredHandler>,
    turn_id: Option<String>,
    error_message: String,
) -> Vec<HookCompletedEvent> {
    handlers
        .into_iter()
        .filter(|handler| matches!(handler.source_path, HandlerSourcePath::Local(_)))
        .map(|handler| {
            let mut run = dispatcher::running_summary(&handler);
            run.status = HookRunStatus::Failed;
            run.completed_at = Some(run.started_at);
            run.duration_ms = Some(0);
            run.entries = vec![HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error_message.clone(),
            }];
            HookCompletedEvent {
                turn_id: turn_id.clone(),
                run,
            }
        })
        .collect()
}

pub(crate) fn serialization_failure_hook_events_for_tool_use(
    handlers: Vec<ConfiguredHandler>,
    turn_id: Option<String>,
    error_message: String,
    tool_use_id: &str,
) -> Vec<HookCompletedEvent> {
    serialization_failure_hook_events(handlers, turn_id, error_message)
        .into_iter()
        .map(|event| hook_completed_for_tool_use(event, tool_use_id))
        .collect()
}

pub(crate) fn hook_completed_for_tool_use(
    mut event: HookCompletedEvent,
    tool_use_id: &str,
) -> HookCompletedEvent {
    event.run = hook_run_for_tool_use(event.run, tool_use_id);
    event
}

pub(crate) fn hook_run_for_tool_use(mut run: HookRunSummary, tool_use_id: &str) -> HookRunSummary {
    run.id = format!("{}:{tool_use_id}", run.id);
    run
}

pub(crate) fn matcher_pattern_for_event(
    event_name: HookEventName,
    matcher: Option<&str>,
) -> Option<&str> {
    match event_name {
        HookEventName::PreToolUse
        | HookEventName::PostToolUse
        | HookEventName::SessionStart
        | HookEventName::SessionEnd
        | HookEventName::SubagentStart
        | HookEventName::SubagentStop
        | HookEventName::PreCompact
        | HookEventName::PostCompact => matcher,
        HookEventName::UserPromptSubmit | HookEventName::Stop | HookEventName::Interrupt => None,
    }
}

pub(crate) fn matches_matcher(
    matcher: Option<&crate::engine::HookMatcher>,
    input: Option<&str>,
) -> bool {
    matcher.is_none_or(|matcher| matcher.matches(input))
}

pub(crate) fn matcher_inputs<'a>(
    tool_name: &'a str,
    matcher_aliases: &'a [String],
) -> Vec<&'a str> {
    // Keep the canonical name first so matcher previews and execution preserve
    // the same primary identity that hook stdin will serialize.
    std::iter::once(tool_name)
        .chain(matcher_aliases.iter().map(String::as_str))
        .collect()
}

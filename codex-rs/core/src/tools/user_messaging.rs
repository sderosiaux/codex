//! Extracts confirmed User Messaging text after input-rewriting hooks.
//! Captures evidence before post-tool hooks.

use crate::tools::context::ToolCallState;
use crate::tools::registry::AnyToolResult;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

/// Matches the retained-message bound applied when history replays this text.
const MAX_DELIVERED_MESSAGE_TOKENS: usize = 900;

/// Captures confirmed delivery before any further await, including post-tool hooks.
pub(super) fn capture_delivery(result: &AnyToolResult, call_state: Option<&ToolCallState>) {
    if let Some(call_state) = call_state
        && let Some(text) = result.delivered_assistant_message()
    {
        let _ = call_state.delivered_assistant_message.set(text);
    }
}

impl AnyToolResult {
    fn delivered_assistant_message(&self) -> Option<String> {
        if !self.result.success_for_logging() {
            return None;
        }
        let payload = self.post_tool_use_payload.as_ref()?;
        // MCP hook names normalize prefixed/unprefixed and flat/namespaced calls.
        // Both connector spellings also have a form for catalogs without connector metadata.
        if !matches!(
            payload.tool_name.name(),
            "mcp__codex_apps__user_messaging__send_message"
                | "mcp__codex_apps__user_messaging_send_message"
                | "mcp__codex_apps__user_message__send_message"
                | "mcp__codex_apps__user_message_send_message"
        ) {
            return None;
        }
        let text = payload.tool_input.get("text")?.as_str()?;
        if text.trim().is_empty() {
            return None;
        }
        Some(truncate_text(
            text,
            TruncationPolicy::Tokens(MAX_DELIVERED_MESSAGE_TOKENS),
        ))
    }
}

#[cfg(test)]
#[path = "user_messaging_tests.rs"]
mod tests;

//! Trace milestones for every tool call handled by core.
//! These events contain call identifiers and names, never arguments or output.

use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::ThreadId;
use codex_tools::ToolName;

pub(crate) fn received(thread_id: ThreadId, tool_name: &ToolName, call_id: &str, turn_id: &str) {
    tracing::event!(
        name: "codex.tool_call_received",
        target: "codex_otel.trace_safe",
        tracing::Level::INFO,
        event.name = "codex.tool_call_received",
        conversation.id = %thread_id,
        turn_id,
        call_id,
        tool_name = tool_name.name.as_str(),
        tool_namespace = namespace(tool_name),
    );
}

pub(crate) fn result_ready(
    thread_id: ThreadId,
    turn_id: &str,
    tool_name: &ToolName,
    call_id: &str,
) {
    tracing::event!(
        name: "codex.tool_result_ready",
        target: "codex_otel.trace_safe",
        tracing::Level::INFO,
        event.name = "codex.tool_result_ready",
        conversation.id = %thread_id,
        turn_id,
        call_id,
        tool_name = tool_name.name.as_str(),
        tool_namespace = namespace(tool_name),
    );
}

fn namespace(tool_name: &ToolName) -> &str {
    tool_name
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty())
        .unwrap_or(DEFAULT_FUNCTION_NAMESPACE)
}

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use codex_config::McpServerConfig;
use codex_file_system::ExecutorFileSystem;
use codex_mcp::McpServerSource;
use codex_mcp::PreparedMcpCall;
use codex_protocol::mcp::CallToolResult;
use codex_tools::ToolName;
use codex_tools::ToolPayload;
use codex_utils_path_uri::PathUri;

use crate::ConversationHistorySnapshot;
use crate::ExtensionData;

/// Future returned by one tool-lifecycle callback.
pub type ToolLifecycleFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Extension-facing outcome for a finished tool call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallOutcome {
    /// The tool returned a normal output.
    Completed {
        /// The tool output's own success marker for telemetry/logging.
        success: bool,
    },
    /// The tool was blocked by host policy before the handler ran.
    Blocked,
    /// The tool did not produce a normal output.
    Failed {
        /// Whether the host reached the tool handler before the failure.
        handler_executed: bool,
    },
    /// The host cancelled the tool before normal completion. Cancellation can
    /// win before the dispatch path accepts the call, so contributors should not
    /// assume a matching start callback exists.
    Aborted,
}

/// Provenance captured from the immutable MCP call selected for one tool invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpToolSource {
    /// An MCP server whose frozen registration matches the active Codex configuration.
    Config,
    /// A compatibility or extension registration without user-owned provenance.
    Other,
}

/// Read-only metadata and provenance captured from the MCP call that will execute.
#[derive(Clone, Debug)]
pub struct McpToolContext {
    tool: crate::McpToolInfo,
    source: McpToolSource,
}

impl McpToolContext {
    /// Snapshots a prepared call without exposing its executable client to extensions.
    ///
    /// Configured servers retain their provenance only when their captured connection
    /// still matches the host configuration for the current tool invocation.
    pub fn from_prepared_call(
        call: &PreparedMcpCall,
        configured_server: Option<&McpServerConfig>,
    ) -> Self {
        let tool = call.tool_info().clone();
        let registration = call.config().mcp_server_catalog.server(call.server_name());
        let source = if registration.is_some_and(|server| {
            matches!(server.source(), McpServerSource::Config)
                && configured_server.is_some_and(|configured| server.config() == configured)
        }) {
            McpToolSource::Config
        } else {
            McpToolSource::Other
        };

        Self { tool, source }
    }

    /// Returns frozen metadata for the exact model-visible MCP tool being executed.
    pub fn tool_info(&self) -> &crate::McpToolInfo {
        &self.tool
    }

    /// Returns the registration source captured with the executable call.
    pub fn source(&self) -> &McpToolSource {
        &self.source
    }
}

/// Input supplied when the host starts executing one tool call.
pub struct ToolStartInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Trusted causal root of the owning turn, absent when unknown or ambiguous.
    pub root_turn_id: Option<&'a str>,
    /// Model-visible tool call id.
    pub call_id: &'a str,
    /// Responses item that issued this call.
    pub originating_item_id: Option<&'a codex_protocol::ResponseItemId>,
    /// Tool name as routed by the host.
    pub tool_name: &'a ToolName,
    /// Read-only metadata and provenance from the exact MCP call that will execute.
    pub mcp_tool: Option<&'a McpToolContext>,
    /// Finalized tool arguments, including any pre-tool-use hook rewrites.
    ///
    /// Payloads can contain sensitive plaintext and must not be logged.
    pub payload: &'a ToolPayload,
    /// Shared read-only snapshot taken after pre-tool hooks have completed.
    pub conversation_history: Arc<dyn ConversationHistorySnapshot>,
}

/// Finalized builtin command and the executor that will run it.
///
/// This runs after command hooks and environment resolution, before command
/// attribution and measurement selection. Filesystem paths belong to the selected
/// executor and must not be interpreted as host-local paths.
pub struct CommandStartInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Host tool call id.
    pub call_id: &'a str,
    /// Resolved argv, including the selected executor's shell wrapper.
    /// Commands can contain sensitive plaintext and must not be logged.
    pub command: &'a [String],
    /// Effective working directory in the selected executor.
    pub cwd: &'a PathUri,
    /// Filesystem of the selected executor.
    pub file_system: &'a dyn ExecutorFileSystem,
}

/// Input supplied after an MCP server responds, before the host reports completion.
pub struct McpToolResultInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Host tool call id, also used in the MCP completion notification.
    pub call_id: &'a str,
    /// Read-only metadata and provenance from the exact MCP call that executed.
    pub mcp_tool: &'a McpToolContext,
    /// Tool arguments after host-side rewriting, including file uploads.
    pub arguments: &'a serde_json::Value,
    /// Server response, including `_meta`. Changes feed the normal client and model output paths.
    ///
    /// Arguments and results can contain sensitive plaintext and must not be logged.
    pub result: &'a mut CallToolResult,
}

/// Input supplied when the host finishes executing one tool call.
pub struct ToolFinishInput<'a> {
    /// Store scoped to the host session runtime.
    pub session_store: &'a ExtensionData,
    /// Store scoped to this thread runtime.
    pub thread_store: &'a ExtensionData,
    /// Store scoped to this turn runtime.
    pub turn_store: &'a ExtensionData,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Model-visible tool call id.
    pub call_id: &'a str,
    /// Tool name as routed by the host.
    pub tool_name: &'a ToolName,
    /// Host-observed result of the tool call.
    pub outcome: ToolCallOutcome,
}

/// A measured tool handler interval: admission through the parallel-execution gate until the
/// outer dispatch completes or is dropped. Includes routing, hooks and result processing;
/// excludes runtime readiness and gate waiting. Also covers cancellation; zero if dispatch
/// never passed the gate. Observers must not block.
pub struct ToolTimingInput<'a> {
    /// Thread that owns the tool call.
    pub thread_id: &'a str,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Model-visible tool call id used to correlate dispatch and timing observations.
    pub call_id: &'a str,
    /// Elapsed handler time.
    pub duration: std::time::Duration,
}

/// A direct call entering dispatch, before readiness, hooks, or policy checks.
/// Observers must return promptly and cannot affect tool execution.
pub struct ToolDispatchInput<'a> {
    /// Thread that owns the tool call.
    pub thread_id: &'a str,
    /// Current turn submission id.
    pub turn_id: &'a str,
    /// Model-visible tool call id used to correlate dispatch and timing observations.
    pub call_id: &'a str,
    /// Tool name supplied to dispatch, before pre-tool hooks run.
    pub tool_name: &'a ToolName,
}

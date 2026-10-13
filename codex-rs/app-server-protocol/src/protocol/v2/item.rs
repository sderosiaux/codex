use super::McpToolCallError;
use super::McpToolCallResult;
use super::UserInput;
use super::shared::v2_enum_from_core;
use crate::JsonSchema;
use crate::TS;
use crate::protocol::item_builders::CommandExecutionPresentation;
use crate::protocol::item_builders::convert_patch_changes;
use crate::protocol::item_builders::review_output_text;
use codex_extension_items::ExtensionItem;
pub use codex_extension_items::image_generation::ImageGenerationFailure;
pub use codex_extension_items::image_generation::ImageGenerationItem;
pub use codex_extension_items::sleep::SleepItem;
pub use codex_extension_items::web_search::WebSearchAction;
pub use codex_extension_items::web_search::WebSearchItem;
use codex_protocol::items::AgentMessageContent as CoreAgentMessageContent;
pub use codex_protocol::items::AgentMessageDelivery;
pub use codex_protocol::items::AsyncUserInputQuestion;
use codex_protocol::items::CollabAgentTool as CoreCollabAgentTool;
use codex_protocol::items::CollabAgentToolCallStatus as CoreCollabAgentToolCallStatus;
use codex_protocol::items::CommandExecutionStatus as CoreCommandExecutionStatus;
use codex_protocol::items::DynamicToolCallStatus as CoreDynamicToolCallStatus;
pub use codex_protocol::items::McpAppDisplayMode;
pub use codex_protocol::items::McpAppUi;
use codex_protocol::items::McpToolCallStatus as CoreMcpToolCallStatus;
use codex_protocol::items::TurnItem as CoreTurnItem;
use codex_protocol::memory_citation::MemoryCitation as CoreMemoryCitation;
use codex_protocol::memory_citation::MemoryCitationEntry as CoreMemoryCitationEntry;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::MessagePhase;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::parse_command::ParsedCommand as CoreParsedCommand;
use codex_protocol::protocol::AgentStatus as CoreAgentStatus;
use codex_protocol::protocol::ExecCommandSource as CoreExecCommandSource;
use codex_protocol::protocol::ExecCommandStatus as CoreExecCommandStatus;
use codex_protocol::protocol::PatchApplyStatus as CorePatchApplyStatus;
use codex_protocol::protocol::SubAgentActivityKind as CoreSubAgentActivityKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::LegacyAppPathString;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_with::serde_as;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum CommandAction {
    Read {
        command: String,
        name: String,
        path: LegacyAppPathString,
    },
    ListFiles {
        command: String,
        path: Option<String>,
    },
    Search {
        command: String,
        query: Option<String>,
        path: Option<String>,
    },
    Unknown {
        command: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryCitation {
    pub entries: Vec<MemoryCitationEntry>,
    pub thread_ids: Vec<String>,
}

impl From<CoreMemoryCitation> for MemoryCitation {
    fn from(value: CoreMemoryCitation) -> Self {
        Self {
            entries: value.entries.into_iter().map(Into::into).collect(),
            thread_ids: value.rollout_ids,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MemoryCitationEntry {
    pub path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub note: String,
}

impl From<CoreMemoryCitationEntry> for MemoryCitationEntry {
    fn from(value: CoreMemoryCitationEntry) -> Self {
        Self {
            path: value.path,
            line_start: value.line_start,
            line_end: value.line_end,
            note: value.note,
        }
    }
}

impl CommandAction {
    pub fn into_core(self) -> CoreParsedCommand {
        match self {
            CommandAction::Read {
                command: cmd,
                name,
                path,
            } => CoreParsedCommand::Read {
                cmd,
                name,
                path: PathBuf::from(path.into_string()),
            },
            CommandAction::ListFiles { command: cmd, path } => {
                CoreParsedCommand::ListFiles { cmd, path }
            }
            CommandAction::Search {
                command: cmd,
                query,
                path,
            } => CoreParsedCommand::Search { cmd, query, path },
            CommandAction::Unknown { command: cmd } => CoreParsedCommand::Unknown { cmd },
        }
    }

    pub fn from_core_with_cwd(value: CoreParsedCommand, cwd: &AbsolutePathBuf) -> Self {
        match value {
            CoreParsedCommand::Read { cmd, name, path } => CommandAction::Read {
                command: cmd,
                name,
                path: cwd.join(path).into(),
            },
            CoreParsedCommand::ListFiles { cmd, path } => {
                CommandAction::ListFiles { command: cmd, path }
            }
            CoreParsedCommand::Search { cmd, query, path } => CommandAction::Search {
                command: cmd,
                query,
                path,
            },
            CoreParsedCommand::Unknown { cmd } => CommandAction::Unknown { command: cmd },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ThreadItem {
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    UserMessage {
        id: String,
        client_id: Option<String>,
        content: Vec<UserInput>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    HookPrompt {
        id: String,
        fragments: Vec<HookPromptFragment>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    AgentMessage {
        id: String,
        text: String,
        #[serde(default)]
        phase: Option<MessagePhase>,
        #[serde(default)]
        memory_citation: Option<MemoryCitation>,
        #[serde(default)]
        delivery: Option<AgentMessageDelivery>,
        #[serde(default)]
        questions: Option<Vec<AsyncUserInputQuestion>>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    FunctionCallOutput {
        id: String,
        name: String,
        namespace: Option<String>,
        output: FunctionCallOutputBody,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    /// EXPERIMENTAL - proposed plan item content. The completed plan item is
    /// authoritative and may not match the concatenation of `PlanDelta` text.
    Plan {
        id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    CommandExecution {
        #[serde(skip)]
        #[schemars(skip)]
        #[ts(skip)]
        model_context: Option<codex_protocol::items::ModelInvocationContext>,
        id: String,
        /// Trusted first-party plugin id when this command resolves to one plugin script.
        #[serde(default)]
        plugin_id: Option<String>,
        /// Safe plugin-relative path when this command resolves to one plugin script.
        #[serde(default)]
        script_path: Option<String>,
        /// The command to be executed.
        command: String,
        /// The command's working directory.
        cwd: LegacyAppPathString,
        /// Identifier for the underlying PTY process (when available).
        process_id: Option<String>,
        #[serde(default)]
        source: CommandExecutionSource,
        status: CommandExecutionStatus,
        /// A best-effort parsing of the command to understand the action(s) it will perform.
        /// This returns a list of CommandAction objects because a single shell command may
        /// be composed of many commands piped together.
        command_actions: Vec<CommandAction>,
        /// The command's output, aggregated from stdout and stderr.
        aggregated_output: Option<String>,
        /// The command's exit code.
        exit_code: Option<i32>,
        /// The duration of the command execution in milliseconds.
        #[ts(type = "number | null")]
        duration_ms: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    FileChange {
        id: String,
        changes: Vec<FileUpdateChange>,
        status: PatchApplyStatus,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    McpToolCall {
        id: String,
        server: String,
        tool: String,
        status: McpToolCallStatus,
        arguments: JsonValue,
        app_context: Option<McpToolCallAppContext>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        /// Legacy compatibility field; prefer `mcpAppUi.resourceUri` when available.
        mcp_app_resource_uri: Option<String>,
        /// Presentation captured from the invoked descriptor; absent in older history.
        mcp_app_ui: Option<McpAppUi>,
        plugin_id: Option<String>,
        read_only_hint: Option<bool>,
        result: Option<Box<McpToolCallResult>>,
        error: Option<McpToolCallError>,
        /// The duration of the MCP tool call in milliseconds.
        #[ts(type = "number | null")]
        duration_ms: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    DynamicToolCall {
        id: String,
        namespace: Option<String>,
        tool: String,
        arguments: JsonValue,
        status: DynamicToolCallStatus,
        content_items: Option<Vec<DynamicToolCallOutputContentItem>>,
        success: Option<bool>,
        /// The duration of the dynamic tool call in milliseconds.
        #[ts(type = "number | null")]
        duration_ms: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    CollabAgentToolCall {
        /// Unique identifier for this collab tool call.
        id: String,
        /// Name of the collab tool that was invoked.
        tool: CollabAgentTool,
        /// Current status of the collab tool call.
        status: CollabAgentToolCallStatus,
        /// Thread ID of the agent issuing the collab request.
        sender_thread_id: String,
        /// Thread ID of the receiving agent, when applicable. In case of spawn operation,
        /// this corresponds to the newly spawned agent.
        receiver_thread_ids: Vec<String>,
        /// Prompt text sent as part of the collab tool call, when available.
        prompt: Option<String>,
        /// Model requested for the spawned agent, when applicable.
        model: Option<String>,
        /// Reasoning effort requested for the spawned agent, when applicable.
        reasoning_effort: Option<ReasoningEffort>,
        /// Last known status of the target agents, when available.
        agents_states: HashMap<String, CollabAgentState>,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    SubAgentActivity {
        /// Resolved model at sub-agent creation; absent from older records and other activities.
        model: Option<String>,
        /// Resolved reasoning effort at sub-agent creation, when known.
        reasoning_effort: Option<ReasoningEffort>,
        id: String,
        kind: SubAgentActivityKind,
        agent_thread_id: String,
        agent_path: String,
    },
    WebSearch(WebSearchItem),
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    ImageView {
        id: String,
        path: LegacyAppPathString,
    },
    Sleep(SleepItem),
    ImageGeneration(ImageGenerationItem),
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    EnteredReviewMode {
        id: String,
        review: String,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    ExitedReviewMode {
        id: String,
        review: String,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    ContextCompaction {
        id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct McpToolCallAppContext {
    pub connector_id: String,
    pub link_id: Option<String>,
    pub resource_uri: Option<String>,
    pub app_name: Option<String>,
    pub action_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct HookPromptFragment {
    pub text: String,
    pub hook_run_id: String,
}

impl ThreadItem {
    pub fn id(&self) -> &str {
        match self {
            ThreadItem::UserMessage { id, .. }
            | ThreadItem::HookPrompt { id, .. }
            | ThreadItem::AgentMessage { id, .. }
            | ThreadItem::FunctionCallOutput { id, .. }
            | ThreadItem::Plan { id, .. }
            | ThreadItem::Reasoning { id, .. }
            | ThreadItem::CommandExecution { id, .. }
            | ThreadItem::FileChange { id, .. }
            | ThreadItem::McpToolCall { id, .. }
            | ThreadItem::DynamicToolCall { id, .. }
            | ThreadItem::CollabAgentToolCall { id, .. }
            | ThreadItem::SubAgentActivity { id, .. }
            | ThreadItem::ImageView { id, .. }
            | ThreadItem::EnteredReviewMode { id, .. }
            | ThreadItem::ExitedReviewMode { id, .. }
            | ThreadItem::ContextCompaction { id, .. } => id,
            ThreadItem::WebSearch(item) => &item.id,
            ThreadItem::Sleep(item) => &item.id,
            ThreadItem::ImageGeneration(item) => &item.id,
        }
    }
}

pub(crate) fn web_search_action_from_core(
    value: codex_protocol::models::WebSearchAction,
) -> WebSearchAction {
    match value {
        codex_protocol::models::WebSearchAction::Search { query, queries } => {
            WebSearchAction::Search { query, queries }
        }
        codex_protocol::models::WebSearchAction::OpenPage { url } => {
            WebSearchAction::OpenPage { url }
        }
        codex_protocol::models::WebSearchAction::FindInPage { url, pattern } => {
            WebSearchAction::FindInPage { url, pattern }
        }
        codex_protocol::models::WebSearchAction::Other => WebSearchAction::Other,
    }
}

impl From<CoreTurnItem> for ThreadItem {
    fn from(value: CoreTurnItem) -> Self {
        match value {
            CoreTurnItem::UserMessage(user) => ThreadItem::UserMessage {
                id: user.id,
                client_id: user.client_id,
                content: user.content.into_iter().map(UserInput::from).collect(),
            },
            CoreTurnItem::HookPrompt(hook_prompt) => ThreadItem::HookPrompt {
                id: hook_prompt.id,
                fragments: hook_prompt
                    .fragments
                    .into_iter()
                    .map(HookPromptFragment::from)
                    .collect(),
            },
            CoreTurnItem::AgentMessage(agent) => {
                let text = agent
                    .content
                    .into_iter()
                    .map(|entry| match entry {
                        CoreAgentMessageContent::Text { text } => text,
                    })
                    .collect::<String>();
                ThreadItem::AgentMessage {
                    id: agent.id,
                    text,
                    phase: agent.phase,
                    memory_citation: agent.memory_citation.map(Into::into),
                    delivery: agent.delivery,
                    questions: agent.questions,
                }
            }
            CoreTurnItem::FunctionCallOutput(output) => ThreadItem::FunctionCallOutput {
                id: output.id,
                name: output.name,
                namespace: output.namespace,
                output: output.output,
            },
            CoreTurnItem::Plan(plan) => ThreadItem::Plan {
                id: plan.id,
                text: plan.text,
            },
            CoreTurnItem::Reasoning(reasoning) => ThreadItem::Reasoning {
                id: reasoning.id,
                summary: reasoning.summary_text,
                content: reasoning.raw_content,
            },
            CoreTurnItem::CommandExecution(command) => {
                let presentation = CommandExecutionPresentation::from_raw(
                    &command.command,
                    &command.parsed_cmd,
                    &command.cwd,
                );
                ThreadItem::CommandExecution {
                    id: command.id,
                    model_context: command.model_context,
                    plugin_id: command.plugin_id,
                    script_path: command.script_path,
                    command: presentation.command,
                    cwd: command.cwd.clone().into(),
                    process_id: command.process_id,
                    source: command.source.into(),
                    status: command.status.into(),
                    command_actions: presentation.command_actions,
                    aggregated_output: command
                        .aggregated_output
                        .filter(|output| !output.is_empty()),
                    exit_code: command.exit_code,
                    duration_ms: command
                        .duration
                        .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
                }
            }
            CoreTurnItem::DynamicToolCall(call) => ThreadItem::DynamicToolCall {
                id: call.id,
                namespace: call.namespace,
                tool: call.tool,
                arguments: call.arguments,
                status: call.status.into(),
                content_items: call.content_items.map(|items| {
                    items
                        .into_iter()
                        .map(DynamicToolCallOutputContentItem::from)
                        .collect()
                }),
                success: call.success,
                duration_ms: call
                    .duration
                    .and_then(|duration| i64::try_from(duration.as_millis()).ok()),
            },
            CoreTurnItem::CollabAgentToolCall(call) => ThreadItem::CollabAgentToolCall {
                id: call.id,
                tool: call.tool.into(),
                status: call.status.into(),
                sender_thread_id: call.sender_thread_id.to_string(),
                receiver_thread_ids: call
                    .receiver_thread_ids
                    .into_iter()
                    .map(String::from)
                    .collect(),
                prompt: call.prompt,
                model: call.model,
                reasoning_effort: call.reasoning_effort,
                agents_states: call
                    .agents_states
                    .into_iter()
                    .map(|(thread_id, status)| (thread_id.to_string(), status.into()))
                    .collect(),
            },
            CoreTurnItem::SubAgentActivity(activity) => ThreadItem::SubAgentActivity {
                model: activity.model,
                reasoning_effort: activity.reasoning_effort,
                id: activity.id,
                kind: activity.kind.into(),
                agent_thread_id: activity.agent_thread_id.to_string(),
                agent_path: String::from(activity.agent_path),
            },
            CoreTurnItem::WebSearch(search) => ThreadItem::WebSearch(WebSearchItem {
                id: search.id,
                query: search.query,
                action: Some(web_search_action_from_core(search.action)),
                results: search.results,
            }),
            CoreTurnItem::ImageView(image) => ThreadItem::ImageView {
                id: image.id,
                path: image.path.into(),
            },
            CoreTurnItem::Extension(extension) => match extension {
                ExtensionItem::ImageGeneration(item) => ThreadItem::ImageGeneration(item),
                ExtensionItem::Sleep(item) => ThreadItem::Sleep(item),
                ExtensionItem::WebSearch(item) => ThreadItem::WebSearch(item),
            },
            CoreTurnItem::ImageGeneration(image) => {
                ThreadItem::ImageGeneration(ImageGenerationItem {
                    id: image.id,
                    status: image.status,
                    revised_prompt: image.revised_prompt,
                    result: image.result,
                    transparent_background: None,
                    failure: None,
                    saved_path: image.saved_path,
                    imagegen_request_id: None,
                    generation_id: None,
                })
            }
            CoreTurnItem::EnteredReviewMode(review) => ThreadItem::EnteredReviewMode {
                id: review.id,
                review: review.user_facing_hint,
            },
            CoreTurnItem::ExitedReviewMode(review) => ThreadItem::ExitedReviewMode {
                id: review.id,
                review: review_output_text(review.review_output.as_ref()),
            },
            CoreTurnItem::FileChange(file_change) => ThreadItem::FileChange {
                id: file_change.id,
                changes: convert_patch_changes(&file_change.changes),
                status: file_change
                    .status
                    .as_ref()
                    .map(PatchApplyStatus::from)
                    .unwrap_or(PatchApplyStatus::InProgress),
            },
            CoreTurnItem::McpToolCall(mcp) => {
                let duration_ms = mcp
                    .duration
                    .and_then(|duration| i64::try_from(duration.as_millis()).ok());

                ThreadItem::McpToolCall {
                    id: mcp.id,
                    server: mcp.server,
                    tool: mcp.tool,
                    status: McpToolCallStatus::from(mcp.status),
                    arguments: mcp.arguments,
                    app_context: mcp.connector_id.map(|connector_id| McpToolCallAppContext {
                        connector_id,
                        link_id: mcp.link_id,
                        resource_uri: mcp.mcp_app_resource_uri.clone(),
                        app_name: mcp.app_name,
                        action_name: mcp.action_name,
                    }),
                    mcp_app_resource_uri: mcp.mcp_app_resource_uri,
                    mcp_app_ui: mcp.mcp_app_ui,
                    plugin_id: mcp.plugin_id,
                    read_only_hint: mcp.read_only_hint,
                    result: mcp.result.map(McpToolCallResult::from).map(Box::new),
                    error: mcp.error.map(McpToolCallError::from),
                    duration_ms,
                }
            }
            CoreTurnItem::ContextCompaction(compaction) => {
                ThreadItem::ContextCompaction { id: compaction.id }
            }
        }
    }
}

impl From<codex_protocol::items::HookPromptFragment> for HookPromptFragment {
    fn from(value: codex_protocol::items::HookPromptFragment) -> Self {
        Self {
            text: value.text,
            hook_run_id: value.hook_run_id,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

impl From<CoreExecCommandStatus> for CommandExecutionStatus {
    fn from(value: CoreExecCommandStatus) -> Self {
        Self::from(&value)
    }
}

impl From<CoreCommandExecutionStatus> for CommandExecutionStatus {
    fn from(value: CoreCommandExecutionStatus) -> Self {
        match value {
            CoreCommandExecutionStatus::InProgress => Self::InProgress,
            CoreCommandExecutionStatus::Completed => Self::Completed,
            CoreCommandExecutionStatus::Failed => Self::Failed,
            CoreCommandExecutionStatus::Declined => Self::Declined,
        }
    }
}

impl From<&CoreExecCommandStatus> for CommandExecutionStatus {
    fn from(value: &CoreExecCommandStatus) -> Self {
        match value {
            CoreExecCommandStatus::Completed => CommandExecutionStatus::Completed,
            CoreExecCommandStatus::Failed => CommandExecutionStatus::Failed,
            CoreExecCommandStatus::Declined => CommandExecutionStatus::Declined,
        }
    }
}

v2_enum_from_core! {
    #[derive(Default)]
    pub enum CommandExecutionSource from CoreExecCommandSource {
        #[default]
        Agent,
        UserShell,
        UnifiedExecStartup,
        UnifiedExecInteraction,
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CollabAgentTool {
    SpawnAgent,
    SendInput,
    ResumeAgent,
    Wait,
    CloseAgent,
    SendMessage,
    FollowupTask,
    InterruptAgent,
    ListAgents,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    pub diff: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update { move_path: Option<PathBuf> },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

impl From<CorePatchApplyStatus> for PatchApplyStatus {
    fn from(value: CorePatchApplyStatus) -> Self {
        Self::from(&value)
    }
}

impl From<&CorePatchApplyStatus> for PatchApplyStatus {
    fn from(value: &CorePatchApplyStatus) -> Self {
        match value {
            CorePatchApplyStatus::Completed => PatchApplyStatus::Completed,
            CorePatchApplyStatus::Failed => PatchApplyStatus::Failed,
            CorePatchApplyStatus::Declined => PatchApplyStatus::Declined,
        }
    }
}

impl From<CoreMcpToolCallStatus> for McpToolCallStatus {
    fn from(value: CoreMcpToolCallStatus) -> Self {
        match value {
            CoreMcpToolCallStatus::InProgress => McpToolCallStatus::InProgress,
            CoreMcpToolCallStatus::Completed => McpToolCallStatus::Completed,
            CoreMcpToolCallStatus::Failed => McpToolCallStatus::Failed,
        }
    }
}

impl From<CoreDynamicToolCallStatus> for DynamicToolCallStatus {
    fn from(value: CoreDynamicToolCallStatus) -> Self {
        match value {
            CoreDynamicToolCallStatus::InProgress => Self::InProgress,
            CoreDynamicToolCallStatus::Completed => Self::Completed,
            CoreDynamicToolCallStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum McpToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum DynamicToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CollabAgentToolCallStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
}

impl From<CoreCollabAgentTool> for CollabAgentTool {
    fn from(value: CoreCollabAgentTool) -> Self {
        match value {
            CoreCollabAgentTool::SpawnAgent => Self::SpawnAgent,
            CoreCollabAgentTool::SendInput => Self::SendInput,
            CoreCollabAgentTool::ResumeAgent => Self::ResumeAgent,
            CoreCollabAgentTool::Wait => Self::Wait,
            CoreCollabAgentTool::CloseAgent => Self::CloseAgent,
            CoreCollabAgentTool::SendMessage => Self::SendMessage,
            CoreCollabAgentTool::FollowupTask => Self::FollowupTask,
            CoreCollabAgentTool::InterruptAgent => Self::InterruptAgent,
            CoreCollabAgentTool::ListAgents => Self::ListAgents,
        }
    }
}

impl From<CoreCollabAgentToolCallStatus> for CollabAgentToolCallStatus {
    fn from(value: CoreCollabAgentToolCallStatus) -> Self {
        match value {
            CoreCollabAgentToolCallStatus::InProgress => Self::InProgress,
            CoreCollabAgentToolCallStatus::Completed => Self::Completed,
            CoreCollabAgentToolCallStatus::Failed => Self::Failed,
            CoreCollabAgentToolCallStatus::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum SubAgentActivityKind {
    Started,
    Interacted,
    Interrupted,
    Completed,
}

impl From<CoreSubAgentActivityKind> for SubAgentActivityKind {
    fn from(value: CoreSubAgentActivityKind) -> Self {
        match value {
            CoreSubAgentActivityKind::Started => SubAgentActivityKind::Started,
            CoreSubAgentActivityKind::Interacted => SubAgentActivityKind::Interacted,
            CoreSubAgentActivityKind::Interrupted => SubAgentActivityKind::Interrupted,
            CoreSubAgentActivityKind::Completed => SubAgentActivityKind::Completed,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum CollabAgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CollabAgentState {
    pub status: CollabAgentStatus,
    pub message: Option<String>,
}

impl From<CoreAgentStatus> for CollabAgentState {
    fn from(value: CoreAgentStatus) -> Self {
        match value {
            CoreAgentStatus::PendingInit => Self {
                status: CollabAgentStatus::PendingInit,
                message: None,
            },
            CoreAgentStatus::Running => Self {
                status: CollabAgentStatus::Running,
                message: None,
            },
            CoreAgentStatus::Interrupted => Self {
                status: CollabAgentStatus::Interrupted,
                message: None,
            },
            CoreAgentStatus::Completed(message) => Self {
                status: CollabAgentStatus::Completed,
                message,
            },
            CoreAgentStatus::Errored(message) => Self {
                status: CollabAgentStatus::Errored,
                message: Some(message),
            },
            CoreAgentStatus::Shutdown => Self {
                status: CollabAgentStatus::Shutdown,
                message: None,
            },
            CoreAgentStatus::NotFound => Self {
                status: CollabAgentStatus::NotFound,
                message: None,
            },
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ItemStartedNotification {
    pub item: ThreadItem,
    pub thread_id: String,
    pub turn_id: String,
    /// Unix timestamp (in milliseconds) when this item lifecycle started.
    #[ts(type = "number")]
    pub started_at_ms: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ItemCompletedNotification {
    pub item: ThreadItem,
    pub thread_id: String,
    pub turn_id: String,
    /// Unix timestamp (in milliseconds) when this item lifecycle completed.
    #[ts(type = "number")]
    pub completed_at_ms: i64,
}

// Item-specific progress notifications

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CommandExecutionOutputDeltaNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct DynamicToolCallResponse {
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum DynamicToolCallOutputContentItem {
    #[serde(rename_all = "camelCase")]
    InputText { text: String },
    #[serde(rename_all = "camelCase")]
    InputImage { image_url: String },
    #[serde(rename_all = "camelCase")]
    InputAudio { audio_url: String },
}

impl From<codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem>
    for DynamicToolCallOutputContentItem
{
    fn from(item: codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem) -> Self {
        match item {
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputText { text } => {
                Self::InputText { text }
            }
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputImage {
                image_url,
            } => Self::InputImage { image_url },
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputAudio {
                audio_url,
            } => Self::InputAudio { audio_url },
        }
    }
}

impl From<DynamicToolCallOutputContentItem>
    for codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem
{
    fn from(item: DynamicToolCallOutputContentItem) -> Self {
        match item {
            DynamicToolCallOutputContentItem::InputText { text } => Self::InputText { text },
            DynamicToolCallOutputContentItem::InputImage { image_url } => {
                Self::InputImage { image_url }
            }
            DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                Self::InputAudio { audio_url }
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL. Defines a single selectable option for request_user_input.
pub struct ToolRequestUserInputOption {
    pub label: String,
    pub description: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL. Represents one request_user_input question and its required options.
pub struct ToolRequestUserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    pub options: Option<Vec<ToolRequestUserInputOption>>,
}

#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
/// EXPERIMENTAL. Params sent with a request_user_input event.
pub struct ToolRequestUserInputParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub questions: Vec<ToolRequestUserInputQuestion>,
    pub is_blocking: bool,
    /// @deprecated Use `isBlocking` to decide whether the request should block.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub auto_resolution_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for ToolRequestUserInputParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WireToolRequestUserInputParams {
            thread_id: String,
            turn_id: String,
            item_id: String,
            questions: Vec<ToolRequestUserInputQuestion>,
            is_blocking: Option<bool>,
            auto_resolution_ms: Option<u64>,
        }

        let wire = WireToolRequestUserInputParams::deserialize(deserializer)?;
        Ok(Self {
            thread_id: wire.thread_id,
            turn_id: wire.turn_id,
            item_id: wire.item_id,
            questions: wire.questions,
            is_blocking: wire.is_blocking.unwrap_or(true),
            auto_resolution_ms: wire.auto_resolution_ms,
        })
    }
}

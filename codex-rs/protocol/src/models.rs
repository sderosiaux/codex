use std::collections::HashMap;

use codex_utils_image::PromptImageMode;
use codex_utils_image::data_url_from_bytes;
use codex_utils_image::load_for_prompt_bytes;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::ser::Serializer;
use serde_with::serde_as;
use ts_rs::TS;

use crate::local_media::audio_mime_for_path;
use crate::user_input::UserInput;
use codex_utils_image::ImageProcessingError;
use schemars::JsonSchema;

use crate::ResponseItemId;
use crate::mcp::CallToolResult;

mod configuration_update;
mod executed_tool_calls;

mod item_metadata;

pub use crate::local_media::MAX_PROMPT_AUDIO_INPUT_BYTES;
pub use crate::local_media::snapshot_local_user_input;
pub use configuration_update::ConfigurationReasoning;
pub use executed_tool_calls::ExecutedToolCall;
pub use executed_tool_calls::ExecutedToolCallArguments;
pub use executed_tool_calls::ExecutedToolCallTruncation;
pub use executed_tool_calls::MAX_TOOL_RESULT_SOURCE_FIELD_BYTES;
pub use executed_tool_calls::ToolResultMetadata;
pub use executed_tool_calls::ToolResultSource;
pub use executed_tool_calls::ToolResultSources;
pub use executed_tool_calls::bound_executed_tool_calls_for_message;
pub use executed_tool_calls::executed_tool_call_metadata_bytes;
pub use executed_tool_calls::normalize_executed_tool_call_completeness;
pub use item_metadata::ContentItemKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseInputItem {
    Message {
        role: String,
        content: Vec<ContentItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        phase: Option<MessagePhase>,
    },
    FunctionCallOutput {
        call_id: String,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
    },
    McpToolCallOutput {
        call_id: String,
        output: CallToolResult,
    },
    CustomToolCallOutput {
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
    },
    ToolSearchOutput {
        call_id: String,
        status: String,
        execution: String,
        #[ts(type = "unknown[]")]
        tools: Vec<serde_json::Value>,
    },
}

#[derive(derive_more::Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    InputText {
        #[debug("{:?} <{} bytes>", &text[..text.floor_char_boundary(/*index*/ 512)], text.len())]
        text: String,
    },
    InputImage {
        #[serde(flatten)]
        image: ImageReference,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },
    InputAudio {
        audio_url: String,
    },
    OutputText {
        text: String,
        // Preserve provider metadata, including empty arrays, for sampled-output replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        #[ts(type = "unknown[]")]
        #[debug(skip)]
        annotations: Option<Vec<serde_json::Value>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        #[ts(type = "unknown[]")]
        #[debug(skip)]
        logprobs: Option<Vec<serde_json::Value>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(untagged)]
#[ts(untagged)]
pub enum ImageReference {
    Inline { image_url: String },
    File { file_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessageInputContent {
    InputText { text: String },
    EncryptedContent { encrypted_content: String },
}

/// Returns the locally readable text when an agent message is entirely plaintext.
pub fn plaintext_agent_message_content(content: &[AgentMessageInputContent]) -> Option<String> {
    let mut text_parts = Vec::with_capacity(content.len());
    for part in content {
        match part {
            AgentMessageInputContent::InputText { text } => text_parts.push(text.as_str()),
            AgentMessageInputContent::EncryptedContent { .. } => return None,
        }
    }

    let text = text_parts.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

pub const DEFAULT_IMAGE_DETAIL: ImageDetail = ImageDetail::High;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
/// Classifies assistant text as commentary, a partial answer, or a terminal answer.
///
/// Providers do not emit this consistently, so callers must treat `None` as
/// "phase unknown" and keep compatibility behavior for legacy models.
pub enum MessagePhase {
    /// Mid-turn assistant text (for example preamble/progress narration).
    ///
    /// Additional tool calls or assistant output may follow before turn
    /// completion.
    Commentary,
    /// Stable answer text that may be followed by more assistant output or tools.
    PartialAnswer,
    /// The assistant's declared terminal answer text for the current turn.
    FinalAnswer,
}

/// Internal Responses API passthrough metadata copied into underlying chat messages.
///
/// Responses API strongly types this payload. Do not modify it without first getting API
/// approval and making the corresponding Responses API change.
#[serde_as]
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
pub struct InternalChatMessageMetadataPassthrough {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    /// Message creation time in fractional Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    #[ts(skip)]
    pub create_time: Option<serde_json::Number>,
    /// Harness-owned classifications aligned with the item's content entries.
    #[serde_as(deserialize_as = "serde_with::DefaultOnError")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    #[ts(skip)]
    pub content_item_kinds: Option<Vec<ContentItemKind>>,
    // Ignore input values so requests and rollout reloads cannot fake tool call records.
    // A resumed thread can record new calls, but cannot prove old calls from this payload.
    /// Host-owned Code Mode cell shared by its `exec` and subsequent `wait` outputs.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    #[ts(skip)]
    pub cell_id: Option<String>,
    /// Warehouse-only Responses metadata, not part of the public app-server protocol.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    #[ts(skip)]
    pub executed_tool_calls: Option<Vec<ExecutedToolCall>>,
    /// Whether the host recorded the complete ordered call inventory without losing calls or names.
    /// Recorded arguments may be truncated independently of this claim.
    /// For a direct tool output this covers its single invocation; with `cell_id`, it covers
    /// the Code Mode cell across its outputs. Neither case describes tool success.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    #[ts(skip)]
    pub tool_calls_complete: Option<bool>,
}

impl InternalChatMessageMetadataPassthrough {
    pub(crate) fn set_turn_id_if_missing(metadata: &mut Option<Self>, turn_id: &str) {
        if turn_id.is_empty()
            || metadata
                .as_ref()
                .and_then(|metadata| metadata.turn_id.as_deref())
                .is_some_and(|turn_id| !turn_id.is_empty())
        {
            return;
        }
        metadata.get_or_insert_with(Self::default).turn_id = Some(turn_id.to_string());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseItem {
    AdditionalTools {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        role: String,
        #[ts(type = "unknown[]")]
        tools: Vec<serde_json::Value>,
    },
    Message {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        role: String,
        content: Vec<ContentItem>,
        // Optional output-message phase (for example: "commentary", "final_answer").
        // Availability varies by provider/model, so downstream consumers must
        // preserve fallback behavior when this is absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        phase: Option<MessagePhase>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    AgentMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        author: String,
        recipient: String,
        content: Vec<AgentMessageInputContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    Reasoning {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        summary: Vec<ReasoningItemReasoningSummary>,
        #[serde(default, skip_serializing_if = "should_serialize_reasoning_content")]
        #[ts(optional)]
        content: Option<Vec<ReasoningItemContent>>,
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    LocalShellCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        /// Legacy id field retained for compatibility with older payloads.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        /// Set when using the Responses API.
        call_id: Option<String>,
        status: LocalShellStatus,
        action: LocalShellAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    FunctionCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        namespace: Option<String>,
        // The Responses API returns the function call arguments as a *string* that contains
        // JSON, not as an already‑parsed object. We keep it as a raw string here and let
        // Session::handle_function_call parse it into a Value.
        arguments: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_function_args: Option<Vec<String>>,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    ToolSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        execution: String,
        #[ts(type = "unknown")]
        arguments: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // NOTE: The `output` field for `function_call_output` uses a dedicated payload type with
    // custom serialization. On the wire it is either:
    //   - a plain string (`content`)
    //   - an array of structured content items (`content_items`)
    // We keep this behavior centralized in `FunctionCallOutputPayload`.
    FunctionCallOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        namespace: Option<String>,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    CustomToolCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,

        call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        namespace: Option<String>,
        input: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // `custom_tool_call_output.output` uses the same wire encoding as
    // `function_call_output.output` so freeform tools can return either plain
    // text or structured content items.
    CustomToolCallOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[ts(as = "FunctionCallOutputBody")]
        #[schemars(with = "FunctionCallOutputBody")]
        output: FunctionCallOutputPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    ToolSearchOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        call_id: Option<String>,
        status: String,
        execution: String,
        #[ts(type = "unknown[]")]
        tools: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // Emitted by the Responses API when the agent triggers a web search.
    // Example payload (from SSE `response.output_item.done`):
    // {
    //   "id":"ws_...",
    //   "type":"web_search_call",
    //   "status":"completed",
    //   "action": {"type":"search","query":"weather: San Francisco, CA"}
    // }
    WebSearchCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        action: Option<WebSearchAction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    // Emitted by the Responses API when the agent triggers image generation.
    // Example payload:
    // {
    //   "id":"ig_123",
    //   "type":"image_generation_call",
    //   "status":"completed",
    //   "revised_prompt":"A gray tabby cat hugging an otter...",
    //   "result":"..."
    // }
    ImageGenerationCall {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        revised_prompt: Option<String>,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    #[serde(alias = "compaction_summary")]
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        encrypted_content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    /// A durable input control interpreted by the backend at its position in history.
    ConfigurationUpdate {
        reasoning: ConfigurationReasoning,
    },
    // Compaction triggers are request controls, not durable response items.
    CompactionTrigger {},
    ContextCompaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        id: Option<ResponseItemId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    },
    #[serde(other)]
    Other,
}

impl ResponseItem {
    /// Clears the top-level `encrypted_content` fields.
    pub fn clear_encrypted_content(&mut self) {
        match self {
            Self::Message {
                encrypted_content, ..
            }
            | Self::Reasoning {
                encrypted_content, ..
            }
            | Self::LocalShellCall {
                encrypted_content, ..
            }
            | Self::FunctionCall {
                encrypted_content, ..
            }
            | Self::ToolSearchCall {
                encrypted_content, ..
            }
            | Self::CustomToolCall {
                encrypted_content, ..
            }
            | Self::WebSearchCall {
                encrypted_content, ..
            }
            | Self::ImageGenerationCall {
                encrypted_content, ..
            }
            | Self::ContextCompaction {
                encrypted_content, ..
            } => *encrypted_content = None,
            Self::Compaction {
                encrypted_content, ..
            } => encrypted_content.clear(),
            Self::AdditionalTools { .. }
            | Self::AgentMessage { .. }
            | Self::FunctionCallOutput { .. }
            | Self::CustomToolCallOutput { .. }
            | Self::ToolSearchOutput { .. }
            | Self::ConfigurationUpdate { .. }
            | Self::CompactionTrigger { .. }
            | Self::Other => {}
        }
    }

    /// Returns the wire type name for this response item.
    pub fn item_type(&self) -> &'static str {
        match self {
            Self::AdditionalTools { .. } => "additional_tools",
            Self::Message { .. } => "message",
            Self::AgentMessage { .. } => "agent_message",
            Self::Reasoning { .. } => "reasoning",
            Self::LocalShellCall { .. } => "local_shell_call",
            Self::FunctionCall { .. } => "function_call",
            Self::ToolSearchCall { .. } => "tool_search_call",
            Self::FunctionCallOutput { .. } => "function_call_output",
            Self::CustomToolCall { .. } => "custom_tool_call",
            Self::CustomToolCallOutput { .. } => "custom_tool_call_output",
            Self::ToolSearchOutput { .. } => "tool_search_output",
            Self::WebSearchCall { .. } => "web_search_call",
            Self::ImageGenerationCall { .. } => "image_generation_call",
            Self::Compaction { .. } => "compaction",
            Self::ConfigurationUpdate { .. } => "configuration_update",
            Self::CompactionTrigger { .. } => "compaction_trigger",
            Self::ContextCompaction { .. } => "context_compaction",
            Self::Other => "other",
        }
    }

    /// Returns whether this item is an ordinary user-role message.
    pub fn is_user_message(&self) -> bool {
        matches!(self, Self::Message { role, .. } if role == "user")
    }

    /// Returns the Responses API item ID, if present.
    pub fn id(&self) -> Option<&ResponseItemId> {
        match self {
            Self::AdditionalTools { id, .. }
            | Self::Message { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::LocalShellCall { id, .. }
            | Self::FunctionCall { id, .. }
            | Self::ToolSearchCall { id, .. }
            | Self::FunctionCallOutput { id, .. }
            | Self::CustomToolCall { id, .. }
            | Self::CustomToolCallOutput { id, .. }
            | Self::ToolSearchOutput { id, .. }
            | Self::WebSearchCall { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ImageGenerationCall { id, .. }
            | Self::Compaction { id, .. }
            | Self::ContextCompaction { id, .. } => id.as_ref(),
            Self::ConfigurationUpdate { .. } | Self::CompactionTrigger { .. } | Self::Other => None,
        }
    }

    /// Sets or clears the Responses API item ID for variants that carry one.
    pub fn set_id(&mut self, new_id: Option<ResponseItemId>) {
        match self {
            Self::AdditionalTools { id, .. }
            | Self::Message { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::LocalShellCall { id, .. }
            | Self::FunctionCall { id, .. }
            | Self::ToolSearchCall { id, .. }
            | Self::FunctionCallOutput { id, .. }
            | Self::CustomToolCall { id, .. }
            | Self::CustomToolCallOutput { id, .. }
            | Self::ToolSearchOutput { id, .. }
            | Self::WebSearchCall { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ImageGenerationCall { id, .. }
            | Self::Compaction { id, .. }
            | Self::ContextCompaction { id, .. } => *id = new_id,
            Self::ConfigurationUpdate { .. } | Self::CompactionTrigger { .. } | Self::Other => {}
        }
    }

    /// Returns the Responses API item ID prefix for variants that carry an ID.
    pub fn id_prefix(&self) -> Option<&'static str> {
        match self {
            Self::AdditionalTools { .. } => Some("at"),
            Self::Message { .. } => Some("msg"),
            Self::AgentMessage { .. } => Some("amsg"),
            Self::Reasoning { .. } => Some("rs"),
            Self::LocalShellCall { .. } => Some("lsh"),
            Self::FunctionCall { .. } => Some("fc"),
            Self::ToolSearchCall { .. } => Some("tsc"),
            Self::FunctionCallOutput { .. } => Some("fco"),
            Self::CustomToolCall { .. } => Some("ctc"),
            Self::CustomToolCallOutput { .. } => Some("ctco"),
            Self::ToolSearchOutput { .. } => Some("tso"),
            Self::WebSearchCall { .. } => Some("ws"),
            Self::ImageGenerationCall { .. } => Some("ig"),
            Self::Compaction { .. } | Self::ContextCompaction { .. } => Some("cmp"),
            Self::ConfigurationUpdate { .. } | Self::CompactionTrigger { .. } | Self::Other => None,
        }
    }

    /// Returns the non-empty turn ID stamped onto this item, if present.
    pub fn turn_id(&self) -> Option<&str> {
        self.internal_chat_message_metadata_passthrough()
            .and_then(|metadata| metadata.turn_id.as_deref())
            .filter(|turn_id| !turn_id.is_empty())
    }

    /// Stamps the item with `turn_id` unless it already has a non-empty turn ID.
    pub fn set_turn_id_if_missing(&mut self, turn_id: &str) {
        let Some(metadata) = self.internal_chat_message_metadata_passthrough_mut() else {
            return;
        };
        InternalChatMessageMetadataPassthrough::set_turn_id_if_missing(metadata, turn_id);
    }

    /// Stamps a harness-authored durable item without replacing its creation time.
    pub fn set_create_time_if_missing(&mut self, create_time: serde_json::Number) {
        let metadata = match self {
            Self::Message {
                role,
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } if matches!(role.as_str(), "user" | "developer") => metadata,
            Self::AgentMessage {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => metadata,
            _ => return,
        };

        metadata
            .get_or_insert_default()
            .create_time
            .get_or_insert(create_time);
    }

    /// Removes internal chat message metadata passthrough before sending to a provider that does
    /// not accept it.
    pub fn clear_internal_chat_message_metadata_passthrough(&mut self) {
        if let Some(metadata) = self.internal_chat_message_metadata_passthrough_mut() {
            *metadata = None;
        }
    }

    /// Removes content item classifications while preserving other passthrough metadata.
    pub fn clear_content_item_kinds(&mut self) {
        let Some(metadata) = self.internal_chat_message_metadata_passthrough_mut() else {
            return;
        };
        let Some(metadata_value) = metadata else {
            return;
        };

        metadata_value.content_item_kinds = None;
        if metadata_value == &InternalChatMessageMetadataPassthrough::default() {
            *metadata = None;
        }
    }

    fn internal_chat_message_metadata_passthrough(
        &self,
    ) -> Option<&InternalChatMessageMetadataPassthrough> {
        match self {
            Self::Message {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::AgentMessage {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Reasoning {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::LocalShellCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::WebSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ImageGenerationCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Compaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ContextCompaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => metadata.as_ref(),
            Self::ConfigurationUpdate { .. }
            | Self::CompactionTrigger { .. }
            | Self::AdditionalTools { .. }
            | Self::Other => None,
        }
    }

    fn internal_chat_message_metadata_passthrough_mut(
        &mut self,
    ) -> Option<&mut Option<InternalChatMessageMetadataPassthrough>> {
        match self {
            Self::Message {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::AgentMessage {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Reasoning {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::LocalShellCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::FunctionCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::CustomToolCallOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ToolSearchOutput {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::WebSearchCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ImageGenerationCall {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::Compaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            }
            | Self::ContextCompaction {
                internal_chat_message_metadata_passthrough: metadata,
                ..
            } => Some(metadata),
            Self::ConfigurationUpdate { .. }
            | Self::CompactionTrigger { .. }
            | Self::AdditionalTools { .. }
            | Self::Other => None,
        }
    }
}

pub const BASE_INSTRUCTIONS_DEFAULT: &str = include_str!("prompts/base_instructions/default.md");

/// Describes whether persisted base instructions were supplied by the user or generated for a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
pub enum BaseInstructionsProvenance {
    /// The instructions were explicitly configured and must survive model changes unchanged.
    Custom,
    /// The instructions were generated from this model's instruction template.
    Model { model: String },
}

/// Base instructions for the model in a thread. Corresponds to the `instructions` field in the ResponsesAPI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename = "base_instructions", rename_all = "snake_case")]
pub struct BaseInstructions {
    pub text: String,
    /// Missing on rollouts written before base-instruction provenance was persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub provenance: Option<BaseInstructionsProvenance>,
}

impl Default for BaseInstructions {
    fn default() -> Self {
        Self {
            text: BASE_INSTRUCTIONS_DEFAULT.to_string(),
            provenance: None,
        }
    }
}

fn should_serialize_reasoning_content(content: &Option<Vec<ReasoningItemContent>>) -> bool {
    match content {
        Some(content) => !content
            .iter()
            .any(|c| matches!(c, ReasoningItemContent::ReasoningText { .. })),
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalMediaKind {
    Audio,
    Image,
}

impl LocalMediaKind {
    fn name(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Image => "image",
        }
    }
}

fn local_media_error_placeholder(
    path: &std::path::Path,
    error: impl std::fmt::Display,
    media_kind: LocalMediaKind,
) -> ContentItem {
    let media_name = media_kind.name();
    let path = path.display();
    ContentItem::InputText {
        text: format!("Codex could not read the local {media_name} at `{path}`: {error}"),
    }
}

pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";

const IMAGE_OPEN_TAG: &str = "<image>";
const IMAGE_CLOSE_TAG: &str = "</image>";
const LOCAL_IMAGE_OPEN_TAG_PREFIX: &str = "<image name=";
const LOCAL_IMAGE_OPEN_TAG_SUFFIX: &str = ">";
const LOCAL_IMAGE_CLOSE_TAG: &str = IMAGE_CLOSE_TAG;
const AUDIO_OPEN_TAG: &str = "<audio>";
const AUDIO_CLOSE_TAG: &str = "</audio>";
const LOCAL_AUDIO_OPEN_TAG_PREFIX: &str = "<audio name=";
const LOCAL_AUDIO_OPEN_TAG_SUFFIX: &str = ">";
const LOCAL_AUDIO_CLOSE_TAG: &str = AUDIO_CLOSE_TAG;

pub fn image_open_tag_text() -> String {
    IMAGE_OPEN_TAG.to_string()
}

pub fn image_close_tag_text() -> String {
    IMAGE_CLOSE_TAG.to_string()
}

pub fn local_image_label_text(label_number: usize) -> String {
    format!("[Image #{label_number}]")
}

pub fn local_image_open_tag_text_with_path(label_number: usize, path: &std::path::Path) -> String {
    let label = local_image_label_text(label_number);
    let path = path.display();
    format!("{LOCAL_IMAGE_OPEN_TAG_PREFIX}{label} path=\"{path}\"{LOCAL_IMAGE_OPEN_TAG_SUFFIX}")
}

pub fn is_local_image_open_tag_text(text: &str) -> bool {
    text.strip_prefix(LOCAL_IMAGE_OPEN_TAG_PREFIX)
        .is_some_and(|rest| rest.ends_with(LOCAL_IMAGE_OPEN_TAG_SUFFIX))
}

pub fn is_local_image_close_tag_text(text: &str) -> bool {
    is_image_close_tag_text(text)
}

pub fn is_image_open_tag_text(text: &str) -> bool {
    text == IMAGE_OPEN_TAG
}

pub fn is_image_close_tag_text(text: &str) -> bool {
    text == IMAGE_CLOSE_TAG
}

pub fn audio_open_tag_text() -> String {
    AUDIO_OPEN_TAG.to_string()
}

pub fn audio_close_tag_text() -> String {
    AUDIO_CLOSE_TAG.to_string()
}

pub fn local_audio_label_text(label_number: usize) -> String {
    format!("[Audio #{label_number}]")
}

pub fn local_audio_open_tag_text_with_path(label_number: usize, path: &std::path::Path) -> String {
    let label = local_audio_label_text(label_number);
    let path = path.display();
    format!("{LOCAL_AUDIO_OPEN_TAG_PREFIX}{label} path=\"{path}\"{LOCAL_AUDIO_OPEN_TAG_SUFFIX}")
}

pub fn is_local_audio_open_tag_text(text: &str) -> bool {
    text.strip_prefix(LOCAL_AUDIO_OPEN_TAG_PREFIX)
        .is_some_and(|rest| rest.ends_with(LOCAL_AUDIO_OPEN_TAG_SUFFIX))
}

pub fn is_local_audio_close_tag_text(text: &str) -> bool {
    is_audio_close_tag_text(text)
}

pub fn is_audio_open_tag_text(text: &str) -> bool {
    text == AUDIO_OPEN_TAG
}

pub fn is_audio_close_tag_text(text: &str) -> bool {
    text == AUDIO_CLOSE_TAG
}

fn invalid_image_error_placeholder(
    path: &std::path::Path,
    error: impl std::fmt::Display,
) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Image located at `{}` is invalid: {}",
            path.display(),
            error
        ),
    }
}

fn unsupported_image_error_placeholder(path: &std::path::Path, mime: &str) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Codex cannot attach image at `{}`: unsupported image `{}`.",
            path.display(),
            mime
        ),
    }
}

pub fn local_image_content_items_with_label_number(
    path: &std::path::Path,
    file_bytes: Vec<u8>,
    label_number: Option<usize>,
    detail: ImageDetail,
) -> Vec<ContentItem> {
    let mode = match detail {
        ImageDetail::Original => PromptImageMode::Original,
        ImageDetail::Auto | ImageDetail::Low | ImageDetail::High => PromptImageMode::ResizeToFit,
    };

    match load_for_prompt_bytes(path, file_bytes, mode) {
        Ok(image) => local_image_content_items(path, image.into_data_url(), label_number, detail),
        Err(err) => match &err {
            ImageProcessingError::Read { .. }
            | ImageProcessingError::Encode { .. }
            | ImageProcessingError::InvalidDataUrl { .. }
            | ImageProcessingError::ImageTooLarge { .. } => {
                vec![local_media_error_placeholder(
                    path,
                    &err,
                    LocalMediaKind::Image,
                )]
            }
            ImageProcessingError::Decode { .. } if err.is_invalid_image() => {
                vec![invalid_image_error_placeholder(path, &err)]
            }
            ImageProcessingError::Decode { .. } => {
                vec![local_media_error_placeholder(
                    path,
                    &err,
                    LocalMediaKind::Image,
                )]
            }
            ImageProcessingError::UnsupportedImageFormat { mime } => {
                vec![unsupported_image_error_placeholder(path, mime)]
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalImagePreparation {
    Process,
    Defer,
}

fn unsupported_audio_error_placeholder(path: &std::path::Path) -> ContentItem {
    ContentItem::InputText {
        text: format!(
            "Codex cannot attach audio at `{}`: unsupported audio format; use wav, mp3, m4a, webm, or ogg.",
            path.display()
        ),
    }
}

fn local_audio_content_items(
    path: &std::path::Path,
    file_bytes: &[u8],
    label_number: usize,
) -> Vec<ContentItem> {
    let Some(mime) = audio_mime_for_path(path) else {
        return vec![unsupported_audio_error_placeholder(path)];
    };

    vec![
        ContentItem::InputText {
            text: local_audio_open_tag_text_with_path(label_number, path),
        },
        ContentItem::InputAudio {
            audio_url: data_url_from_bytes(mime, file_bytes),
        },
        ContentItem::InputText {
            text: LOCAL_AUDIO_CLOSE_TAG.to_string(),
        },
    ]
}

fn local_image_content_items(
    path: &std::path::Path,
    image_url: String,
    label_number: Option<usize>,
    detail: ImageDetail,
) -> Vec<ContentItem> {
    let mut items = Vec::with_capacity(3);
    if let Some(label_number) = label_number {
        items.push(ContentItem::InputText {
            text: local_image_open_tag_text_with_path(label_number, path),
        });
    }
    items.push(ContentItem::InputImage {
        image: ImageReference::Inline { image_url },
        detail: Some(detail),
    });
    if label_number.is_some() {
        items.push(ContentItem::InputText {
            text: LOCAL_IMAGE_CLOSE_TAG.to_string(),
        });
    }
    items
}

impl From<ResponseInputItem> for ResponseItem {
    fn from(item: ResponseInputItem) -> Self {
        match item {
            ResponseInputItem::Message {
                role,
                content,
                phase,
            } => Self::Message {
                status: None,
                encrypted_content: None,
                role,
                content,
                id: None,
                phase,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::FunctionCallOutput { call_id, output } => Self::FunctionCallOutput {
                id: None,
                call_id: Some(call_id),
                name: None,
                namespace: None,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::McpToolCallOutput { call_id, output } => {
                let output = output.into_function_call_output_payload();
                Self::FunctionCallOutput {
                    id: None,
                    call_id: Some(call_id),
                    name: None,
                    namespace: None,
                    output,
                    internal_chat_message_metadata_passthrough: None,
                }
            }
            ResponseInputItem::CustomToolCallOutput {
                call_id,
                name,
                output,
            } => Self::CustomToolCallOutput {
                id: None,
                call_id,
                name,
                output,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseInputItem::ToolSearchOutput {
                call_id,
                status,
                execution,
                tools,
            } => Self::ToolSearchOutput {
                call_id: Some(call_id),
                status,
                execution,
                tools,
                id: None,
                internal_chat_message_metadata_passthrough: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum LocalShellStatus {
    Completed,
    InProgress,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalShellAction {
    Exec(LocalShellExecAction),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
pub struct LocalShellExecAction {
    pub command: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub working_directory: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[schemars(rename = "ResponsesApiWebSearchAction")]
pub enum WebSearchAction {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        query: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        pattern: Option<String>,
    },

    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningItemReasoningSummary {
    SummaryText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningItemContent {
    ReasoningText { text: String },
    Text { text: String },
}

impl From<Vec<UserInput>> for ResponseInputItem {
    fn from(items: Vec<UserInput>) -> Self {
        Self::from_user_input(items, LocalImagePreparation::Process, &mut HashMap::new())
    }
}

impl ResponseInputItem {
    /// Records original user-input indices to image slots in the generated message content.
    /// Inputs that do not produce an image, including failed local reads, have no entry.
    pub fn from_user_input(
        items: Vec<UserInput>,
        local_image_preparation: LocalImagePreparation,
        user_image_content_indices: &mut HashMap<usize, usize>,
    ) -> Self {
        user_image_content_indices.clear();
        let mut image_index = 0;
        let mut audio_index = 0;
        let mut content = Vec::new();
        for (input_index, input) in items.into_iter().enumerate() {
            let generated = match input {
                UserInput::Text { text, .. } => vec![ContentItem::InputText { text }],
                UserInput::Image { image, detail, .. } => {
                    image_index += 1;
                    let detail = detail.unwrap_or(DEFAULT_IMAGE_DETAIL);
                    vec![ContentItem::InputImage {
                        image,
                        detail: Some(detail),
                    }]
                }
                UserInput::LocalImage { path, detail, .. } => {
                    image_index += 1;
                    let detail = detail.unwrap_or(DEFAULT_IMAGE_DETAIL);
                    match std::fs::read(&path) {
                        Ok(file_bytes) => match local_image_preparation {
                            LocalImagePreparation::Process => {
                                local_image_content_items_with_label_number(
                                    &path,
                                    file_bytes,
                                    Some(image_index),
                                    detail,
                                )
                            }
                            LocalImagePreparation::Defer => local_image_content_items(
                                &path,
                                data_url_from_bytes("application/octet-stream", &file_bytes),
                                Some(image_index),
                                detail,
                            ),
                        },
                        Err(err) => vec![local_media_error_placeholder(
                            &path,
                            err,
                            LocalMediaKind::Image,
                        )],
                    }
                }
                UserInput::Audio { audio_url } => {
                    audio_index += 1;
                    vec![ContentItem::InputAudio { audio_url }]
                }
                UserInput::LocalAudio { path } => {
                    audio_index += 1;
                    match std::fs::read(&path) {
                        Ok(file_bytes) => {
                            local_audio_content_items(&path, &file_bytes, audio_index)
                        }
                        Err(err) => vec![local_media_error_placeholder(
                            &path,
                            err,
                            LocalMediaKind::Audio,
                        )],
                    }
                }
                UserInput::Skill { .. } | UserInput::Mention { .. } => Vec::new(), // Tool bodies are injected later in core
            };
            // Local images add framing text, while mentions and skills add no content.
            // Capture the image's actual slot before preparation replaces its reference.
            for (offset, item) in generated.iter().enumerate() {
                if matches!(item, ContentItem::InputImage { .. }) {
                    user_image_content_indices.insert(input_index, content.len() + offset);
                }
            }
            content.extend(generated);
        }
        Self::Message {
            role: "user".to_string(),
            content,
            phase: None,
        }
    }
}
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
pub struct SearchToolCallParams {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub limit: Option<usize>,
}

/// Responses API compatible content items that can be returned by a tool call.
/// This is a subset of ContentItem with the types we support as function call outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FunctionCallOutputContentItem {
    // Do not rename, these are serialized and used directly in the responses API.
    InputText {
        text: String,
    },
    // Do not rename, these are serialized and used directly in the responses API.
    InputImage {
        #[serde(flatten)]
        image: ImageReference,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<ImageDetail>,
    },
    // Do not rename, these are serialized and used directly in the responses API.
    InputAudio {
        audio_url: String,
    },
    EncryptedContent {
        encrypted_content: String,
    },
}

/// Converts structured function-call output content into plain text for
/// human-readable surfaces.
///
/// This conversion is intentionally lossy:
/// - only `input_text` items are included
/// - image and audio items are ignored
///
/// We use this helper where callers still need a string representation (for
/// example telemetry previews or legacy string-only output paths) while keeping
/// the original multimodal `content_items` as the authoritative payload sent to
/// the model.
pub fn function_call_output_content_items_to_text(
    content_items: &[FunctionCallOutputContentItem],
) -> Option<String> {
    let text_segments = content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
                Some(text.as_str())
            }
            FunctionCallOutputContentItem::InputText { .. }
            | FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::InputAudio { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
        })
        .collect::<Vec<_>>();

    if text_segments.is_empty() {
        None
    } else {
        Some(text_segments.join("\n"))
    }
}

impl From<crate::dynamic_tools::DynamicToolCallOutputContentItem>
    for FunctionCallOutputContentItem
{
    fn from(item: crate::dynamic_tools::DynamicToolCallOutputContentItem) -> Self {
        match item {
            crate::dynamic_tools::DynamicToolCallOutputContentItem::InputText { text } => {
                Self::InputText { text }
            }
            crate::dynamic_tools::DynamicToolCallOutputContentItem::InputImage { image_url } => {
                Self::InputImage {
                    image: ImageReference::Inline { image_url },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                }
            }
            crate::dynamic_tools::DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                Self::InputAudio { audio_url }
            }
        }
    }
}

/// The payload we send back to OpenAI when reporting a tool call result.
///
/// `body` serializes directly as the wire value for `function_call_output.output`.
/// `success` remains internal metadata for downstream handling.
#[derive(Debug, Default, Clone, PartialEq, JsonSchema, TS)]
pub struct FunctionCallOutputPayload {
    pub body: FunctionCallOutputBody,
    pub success: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(untagged)]
pub enum FunctionCallOutputBody {
    Text(String),
    ContentItems(Vec<FunctionCallOutputContentItem>),
}

impl FunctionCallOutputBody {
    /// Best-effort conversion of a function-call output body to plain text for
    /// human-readable surfaces.
    ///
    /// This conversion is intentionally lossy when the body contains content
    /// items: image entries are dropped and text entries are joined with
    /// newlines.
    pub fn to_text(&self) -> Option<String> {
        match self {
            Self::Text(content) => Some(content.clone()),
            Self::ContentItems(items) => function_call_output_content_items_to_text(items),
        }
    }
}

impl Default for FunctionCallOutputBody {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl FunctionCallOutputPayload {
    pub fn from_text(content: String) -> Self {
        Self {
            body: FunctionCallOutputBody::Text(content),
            success: None,
        }
    }

    pub fn from_content_items(content_items: Vec<FunctionCallOutputContentItem>) -> Self {
        Self {
            body: FunctionCallOutputBody::ContentItems(content_items),
            success: None,
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        match &self.body {
            FunctionCallOutputBody::Text(content) => Some(content),
            FunctionCallOutputBody::ContentItems(_) => None,
        }
    }

    pub fn content_items(&self) -> Option<&[FunctionCallOutputContentItem]> {
        match &self.body {
            FunctionCallOutputBody::Text(_) => None,
            FunctionCallOutputBody::ContentItems(items) => Some(items),
        }
    }

    pub fn content_items_mut(&mut self) -> Option<&mut Vec<FunctionCallOutputContentItem>> {
        match &mut self.body {
            FunctionCallOutputBody::Text(_) => None,
            FunctionCallOutputBody::ContentItems(items) => Some(items),
        }
    }
}

// `function_call_output.output` is encoded as either:
//   - an array of structured content items
//   - a plain string
impl Serialize for FunctionCallOutputPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.body {
            FunctionCallOutputBody::Text(content) => serializer.serialize_str(content),
            FunctionCallOutputBody::ContentItems(items) => items.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for FunctionCallOutputPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let body = FunctionCallOutputBody::deserialize(deserializer)?;
        Ok(FunctionCallOutputPayload {
            body,
            success: None,
        })
    }
}

impl CallToolResult {
    pub fn from_result(result: Result<Self, String>) -> Self {
        match result {
            Ok(result) => result,
            Err(error) => Self::from_error_text(error),
        }
    }

    pub fn from_error_text(text: String) -> Self {
        Self {
            content: vec![serde_json::json!({
                "type": "text",
                "text": text,
            })],
            structured_content: None,
            is_error: Some(true),
            meta: None,
        }
    }

    pub fn success(&self) -> bool {
        self.is_error != Some(true)
    }

    pub fn as_function_call_output_payload(&self) -> FunctionCallOutputPayload {
        let content_items = convert_mcp_content_to_items(&self.content);
        if content_items
            .iter()
            .any(|item| matches!(item, FunctionCallOutputContentItem::EncryptedContent { .. }))
        {
            return FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(content_items),
                success: Some(self.success()),
            };
        }

        if let Some(structured_content) = &self.structured_content
            && !structured_content.is_null()
        {
            match serde_json::to_string(structured_content) {
                Ok(serialized_structured_content) => {
                    return FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text(serialized_structured_content),
                        success: Some(self.success()),
                    };
                }
                Err(err) => {
                    return FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text(err.to_string()),
                        success: Some(false),
                    };
                }
            }
        }

        FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(content_items),
            success: Some(self.success()),
        }
    }

    pub fn into_function_call_output_payload(self) -> FunctionCallOutputPayload {
        self.as_function_call_output_payload()
    }
}

fn convert_mcp_content_to_items(
    contents: &[serde_json::Value],
) -> Vec<FunctionCallOutputContentItem> {
    const CODEX_ENCRYPTED_CONTENT_META_KEY: &str = "codex/encryptedContent";
    const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";

    #[derive(serde::Deserialize)]
    #[serde(tag = "type")]
    enum McpContent {
        #[serde(rename = "text")]
        Text {
            text: String,
            #[serde(rename = "_meta", default)]
            meta: Option<serde_json::Value>,
        },
        #[serde(rename = "image")]
        Image {
            data: String,
            #[serde(rename = "mimeType", alias = "mime_type")]
            mime_type: Option<String>,
            #[serde(rename = "_meta", default)]
            meta: Option<serde_json::Value>,
        },
        #[serde(rename = "audio")]
        Audio {
            data: String,
            #[serde(rename = "mimeType", alias = "mime_type")]
            mime_type: Option<String>,
            #[serde(rename = "_meta", default)]
            _meta: Option<serde_json::Value>,
        },
        #[serde(other)]
        Unknown,
    }

    let mut items = Vec::with_capacity(contents.len());

    for content in contents {
        let item = match serde_json::from_value::<McpContent>(content.clone()) {
            Ok(McpContent::Text { text, meta }) => {
                if meta
                    .as_ref()
                    .and_then(|meta| meta.get(CODEX_ENCRYPTED_CONTENT_META_KEY))
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    FunctionCallOutputContentItem::EncryptedContent {
                        encrypted_content: text,
                    }
                } else {
                    FunctionCallOutputContentItem::InputText { text }
                }
            }
            Ok(McpContent::Image {
                data,
                mime_type,
                meta,
            }) => {
                let image_url = if data.starts_with("data:") {
                    data
                } else {
                    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
                    format!("data:{mime_type};base64,{data}")
                };
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url },
                    detail: meta
                        .as_ref()
                        .and_then(serde_json::Value::as_object)
                        .and_then(|meta| meta.get(CODEX_IMAGE_DETAIL_META_KEY))
                        .and_then(serde_json::Value::as_str)
                        .and_then(|detail| match detail {
                            "auto" => Some(ImageDetail::Auto),
                            "low" => Some(ImageDetail::Low),
                            "high" => Some(ImageDetail::High),
                            "original" => Some(ImageDetail::Original),
                            _ => None,
                        })
                        .or(Some(DEFAULT_IMAGE_DETAIL)),
                }
            }
            Ok(McpContent::Audio {
                data, mime_type, ..
            }) => {
                let audio_url = if data.starts_with("data:") {
                    data
                } else {
                    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
                    format!("data:{mime_type};base64,{data}")
                };
                FunctionCallOutputContentItem::InputAudio { audio_url }
            }
            Ok(McpContent::Unknown) | Err(_) => FunctionCallOutputContentItem::InputText {
                text: serde_json::to_string(content).unwrap_or_else(|_| "<content>".to_string()),
            },
        };
        items.push(item);
    }

    items
}

// Implement Display so callers can treat the payload like a plain string when logging or doing
// trivial substring checks in tests (existing tests call `.contains()` on the output). For
// `ContentItems`, Display emits a JSON representation.

impl std::fmt::Display for FunctionCallOutputPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.body {
            FunctionCallOutputBody::Text(content) => f.write_str(content),
            FunctionCallOutputBody::ContentItems(items) => {
                let content = serde_json::to_string(items).unwrap_or_default();
                f.write_str(content.as_str())
            }
        }
    }
}

// (Moved event mapping logic into codex-core to avoid coupling protocol to UI-facing events.)

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // A tiny valid PNG (1x1) so image conversion tests don't depend on cross-crate
    // file paths, which break under Bazel sandboxing.
    const TINY_PNG_BYTES: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 122, 94, 171, 63, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    #[test]
    fn base_instructions_preserve_provenance_and_accept_legacy_rollouts() -> Result<()> {
        let legacy: BaseInstructions =
            serde_json::from_value(serde_json::json!({ "text": "legacy instructions" }))?;
        assert_eq!(legacy.provenance, None);

        for provenance in [
            BaseInstructionsProvenance::Custom,
            BaseInstructionsProvenance::Model {
                model: "gpt-5.2".to_string(),
            },
        ] {
            let instructions = BaseInstructions {
                text: "persisted instructions".to_string(),
                provenance: Some(provenance),
            };
            assert_eq!(
                serde_json::from_value::<BaseInstructions>(serde_json::to_value(&instructions)?)?,
                instructions
            );
        }

        Ok(())
    }

    #[test]
    fn plaintext_agent_message_content_rejects_mixed_encrypted_content() {
        let content = vec![
            AgentMessageInputContent::InputText {
                text: "Message Type: MESSAGE\nPayload:\n".to_string(),
            },
            AgentMessageInputContent::EncryptedContent {
                encrypted_content: "encrypted-payload".to_string(),
            },
        ];

        assert_eq!(plaintext_agent_message_content(&content), None);
    }

    #[test]
    fn response_input_message_conversion_preserves_phase() -> Result<()> {
        for phase in [
            None,
            Some(MessagePhase::Commentary),
            Some(MessagePhase::PartialAnswer),
            Some(MessagePhase::FinalAnswer),
        ] {
            let input = ResponseInputItem::Message {
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: "answer text".to_string(),
                }],
                phase: phase.clone(),
            };
            let wire = serde_json::to_value(input)?;
            let item = ResponseItem::from(serde_json::from_value::<ResponseInputItem>(wire)?);
            let expected = ResponseItem::Message {
                status: None,
                encrypted_content: None,
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: "answer text".to_string(),
                }],
                phase,
                internal_chat_message_metadata_passthrough: None,
            };
            assert_eq!(item, expected);
            assert_eq!(
                serde_json::from_value::<ResponseItem>(serde_json::to_value(item)?)?,
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn response_item_passthrough_metadata_round_trips_and_stamps_turn_ids() -> Result<()> {
        let mut item =
            response_item_with_passthrough_metadata(Some(passthrough_metadata("turn-1")));
        let round_trip: ResponseItem = serde_json::from_value(serde_json::to_value(&item)?)?;
        assert_eq!(round_trip, item);

        let unknown_metadata: ResponseItem = serde_json::from_value(serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}],
            "internal_chat_message_metadata_passthrough": {
                "turn_id": "turn-1",
                "other": "ignored",
            },
        }))?;
        assert_eq!(unknown_metadata, item);

        item.append_executed_tool_calls(vec![ExecutedToolCall::new(
            "test_tool".to_string(),
            serde_json::json!({"value": 1}),
        )]);
        let serialized = serde_json::to_value(&item)?;
        assert_eq!(
            serialized["internal_chat_message_metadata_passthrough"]["executed_tool_calls"],
            serde_json::json!([{"name": "test_tool", "arguments": {"value": 1}}]),
        );
        let deserialized = serde_json::from_value::<ResponseItem>(serialized)?;
        assert_eq!(deserialized.turn_id(), Some("turn-1"));
        assert!(
            deserialized
                .executed_tool_call_metadata()
                .and_then(|metadata| metadata.executed_tool_calls.as_ref())
                .is_none(),
            "provider and rollout input cannot forge local attempted-tool metadata",
        );
        item.clear_executed_tool_calls();
        item.set_turn_id_if_missing("turn-2");
        assert_eq!(item.turn_id(), Some("turn-1"));

        let mut empty_turn_id =
            response_item_with_passthrough_metadata(Some(passthrough_metadata("")));
        empty_turn_id.set_turn_id_if_missing("turn-1");
        assert_eq!(empty_turn_id.turn_id(), Some("turn-1"));

        let mut missing_turn_id = response_item_with_passthrough_metadata(
            /*internal_chat_message_metadata_passthrough*/ None,
        );
        let create_time = serde_json::Number::from(123);
        missing_turn_id.set_create_time_if_missing(create_time.clone());
        missing_turn_id.set_turn_id_if_missing("");
        missing_turn_id.set_turn_id_if_missing("turn-1");
        missing_turn_id.set_create_time_if_missing(serde_json::Number::from(456));
        assert_eq!(
            missing_turn_id.executed_tool_call_metadata(),
            Some(&InternalChatMessageMetadataPassthrough {
                turn_id: Some("turn-1".to_string()),
                create_time: Some(create_time),
                ..Default::default()
            })
        );

        let mut other = ResponseItem::Other;
        other.set_turn_id_if_missing("turn-1");
        assert_eq!(other.turn_id(), None);
        Ok(())
    }

    #[test]
    fn response_item_id_getter_and_setter() {
        let mut item = response_item_with_passthrough_metadata(
            /*internal_chat_message_metadata_passthrough*/ None,
        );
        assert_eq!(item.id(), None);

        item.set_id(Some(ResponseItemId::with_suffix("msg", "test")));

        assert_eq!(item.id().map(ResponseItemId::as_str), Some("msg_test"));

        item.set_id(/*new_id*/ None);

        assert_eq!(item.id(), None);

        let mut additional_tools = ResponseItem::AdditionalTools {
            id: None,
            role: "developer".to_string(),
            tools: Vec::new(),
        };
        additional_tools.set_id(Some(ResponseItemId::with_suffix("at", "test")));
        assert_eq!(
            additional_tools.id().map(ResponseItemId::as_str),
            Some("at_test")
        );
    }

    fn response_item_with_passthrough_metadata(
        internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    ) -> ResponseItem {
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough,
        }
    }

    fn passthrough_metadata(turn_id: &str) -> InternalChatMessageMetadataPassthrough {
        InternalChatMessageMetadataPassthrough {
            turn_id: Some(turn_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn image_detail_roundtrips_all_wire_values() -> Result<()> {
        assert_eq!(
            serde_json::from_str::<ImageDetail>("\"auto\"")?,
            ImageDetail::Auto
        );
        assert_eq!(
            serde_json::from_str::<ImageDetail>("\"low\"")?,
            ImageDetail::Low
        );
        assert_eq!(serde_json::to_string(&ImageDetail::Auto)?, "\"auto\"");
        assert_eq!(serde_json::to_string(&ImageDetail::Low)?, "\"low\"");

        let content_item: ContentItem = serde_json::from_value(serde_json::json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,abc",
            "detail": "auto",
        }))?;

        assert_eq!(
            content_item,
            ContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,abc".to_string(),
                },
                detail: Some(ImageDetail::Auto),
            }
        );

        Ok(())
    }

    #[test]
    fn convert_mcp_content_to_items_preserves_data_urls() {
        let contents = vec![serde_json::json!({
            "type": "image",
            "data": "data:image/png;base64,Zm9v",
            "mimeType": "image/png",
        })];

        let items = convert_mcp_content_to_items(&contents);
        assert_eq!(
            items,
            vec![FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,Zm9v".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            }]
        );
    }

    #[test]
    fn response_item_parses_image_generation_call() {
        let item = serde_json::from_value::<ResponseItem>(serde_json::json!({
            "id": "ig_123",
            "type": "image_generation_call",
            "status": "completed",
            "revised_prompt": "A small blue square",
            "result": "Zm9v",
        }))
        .expect("image generation item should deserialize");

        assert_eq!(
            item,
            ResponseItem::ImageGenerationCall {
                encrypted_content: None,
                id: Some(ResponseItemId::with_suffix("ig", "123")),
                status: "completed".to_string(),
                revised_prompt: Some("A small blue square".to_string()),
                result: "Zm9v".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }
        );
    }

    #[test]
    fn response_item_parses_image_generation_call_without_revised_prompt() {
        let item = serde_json::from_value::<ResponseItem>(serde_json::json!({
            "id": "ig_123",
            "type": "image_generation_call",
            "status": "completed",
            "result": "Zm9v",
        }))
        .expect("image generation item should deserialize");

        assert_eq!(
            item,
            ResponseItem::ImageGenerationCall {
                encrypted_content: None,
                id: Some(ResponseItemId::with_suffix("ig", "123")),
                status: "completed".to_string(),
                revised_prompt: None,
                result: "Zm9v".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }
        );
    }

    #[test]
    fn convert_mcp_content_to_items_builds_data_urls_when_missing_prefix() {
        let contents = vec![serde_json::json!({
            "type": "image",
            "data": "Zm9v",
            "mimeType": "image/png",
        })];

        let items = convert_mcp_content_to_items(&contents);
        assert_eq!(
            items,
            vec![FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,Zm9v".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            }]
        );
    }

    #[test]
    fn convert_mcp_audio_content_builds_data_urls_and_preserves_existing_data_urls() {
        let contents = vec![
            serde_json::json!({
                "type": "audio",
                "data": "Zm9v",
                "mimeType": "audio/wav",
                "_meta": {"source": "microphone"},
            }),
            serde_json::json!({
                "type": "audio",
                "data": "data:audio/ogg;base64,YmFy",
                "mimeType": "audio/ogg",
            }),
        ];

        assert_eq!(
            convert_mcp_content_to_items(&contents),
            vec![
                FunctionCallOutputContentItem::InputAudio {
                    audio_url: "data:audio/wav;base64,Zm9v".to_string(),
                },
                FunctionCallOutputContentItem::InputAudio {
                    audio_url: "data:audio/ogg;base64,YmFy".to_string(),
                },
            ]
        );
    }

    #[test]
    fn convert_mcp_content_to_items_converts_text_without_media() {
        let contents = vec![serde_json::json!({
            "type": "text",
            "text": "hello",
        })];

        assert_eq!(
            convert_mcp_content_to_items(&contents),
            vec![FunctionCallOutputContentItem::InputText {
                text: "hello".to_string(),
            }]
        );
    }

    #[test]
    fn function_call_output_content_items_to_text_joins_text_segments() {
        let content_items = vec![
            FunctionCallOutputContentItem::InputText {
                text: "line 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AAA".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: "line 2".to_string(),
            },
        ];

        let text = function_call_output_content_items_to_text(&content_items);
        assert_eq!(text, Some("line 1\nline 2".to_string()));
    }

    #[test]
    fn function_call_output_content_items_to_text_ignores_blank_text_and_media() {
        let content_items = vec![
            FunctionCallOutputContentItem::InputText {
                text: "   ".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AAA".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputAudio {
                audio_url: "data:audio/wav;base64,AAA".to_string(),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_opaque".to_string(),
            },
        ];

        let text = function_call_output_content_items_to_text(&content_items);
        assert_eq!(text, None);
    }

    #[test]
    fn function_call_output_body_to_text_returns_plain_text_content() {
        let body = FunctionCallOutputBody::Text("ok".to_string());
        let text = body.to_text();
        assert_eq!(text, Some("ok".to_string()));
    }

    #[test]
    fn function_call_output_body_to_text_uses_content_item_fallback() {
        let body = FunctionCallOutputBody::ContentItems(vec![
            FunctionCallOutputContentItem::InputText {
                text: "line 1".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,AAA".to_string(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]);

        let text = body.to_text();
        assert_eq!(text, Some("line 1".to_string()));
    }

    #[test]
    fn function_call_deserializes_optional_namespace() {
        let item: ResponseItem = serde_json::from_value(serde_json::json!({
            "type": "function_call",
            "name": "mcp__codex_apps__gmail_get_recent_emails",
            "namespace": "mcp__codex_apps__gmail",
            "arguments": "{\"top_k\":5}",
            "call_id": "call-1",
        }))
        .expect("function_call should deserialize");

        assert_eq!(
            item,
            ResponseItem::FunctionCall {
                status: None,
                encrypted_content: None,
                id: None,
                name: "mcp__codex_apps__gmail_get_recent_emails".to_string(),
                namespace: Some("mcp__codex_apps__gmail".to_string()),
                arguments: "{\"top_k\":5}".to_string(),
                encrypted_function_args: None,
                call_id: "call-1".to_string(),
                internal_chat_message_metadata_passthrough: None,
            }
        );
    }

    #[test]
    fn paired_function_call_output_preserves_existing_wire_shape() {
        let value = serde_json::json!({
            "type": "function_call_output",
            "call_id": "call-1",
            "output": "done",
        });
        let item: ResponseItem = serde_json::from_value(value.clone())
            .expect("paired function call output should deserialize");

        assert_eq!(
            item,
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: Some("call-1".to_string()),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text("done".to_string()),
                internal_chat_message_metadata_passthrough: None,
            }
        );
        assert_eq!(serde_json::to_value(item).expect("serialize item"), value);
    }

    #[test]
    fn named_unpaired_function_call_output_round_trips_without_call_id() {
        let value = serde_json::json!({
            "type": "function_call_output",
            "name": "notifications",
            "namespace": "slack",
            "output": "Alice mentioned you.",
        });
        let item: ResponseItem = serde_json::from_value(value.clone())
            .expect("named unpaired function call output should deserialize");

        assert_eq!(
            item,
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: None,
                name: Some("notifications".to_string()),
                namespace: Some("slack".to_string()),
                output: FunctionCallOutputPayload::from_text("Alice mentioned you.".to_string()),
                internal_chat_message_metadata_passthrough: None,
            }
        );
        assert_eq!(serde_json::to_value(item).expect("serialize item"), value);
    }

    #[test]
    fn function_call_preserves_empty_encrypted_function_args() {
        let value = serde_json::json!({
            "type": "function_call",
            "name": "spawn_agent",
            "namespace": "collaboration",
            "arguments": "{\"message\":\"hello\",\"task_name\":\"worker\"}",
            "encrypted_function_args": [],
            "call_id": "call-1",
        });
        let item: ResponseItem =
            serde_json::from_value(value.clone()).expect("function_call should deserialize");

        assert_eq!(serde_json::to_value(item).expect("serialize item"), value);
    }

    #[test]
    fn serializes_success_as_plain_string() -> Result<()> {
        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call1".into(),
            output: FunctionCallOutputPayload::from_text("ok".into()),
        };

        let json = serde_json::to_string(&item)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;

        // Success case -> output should be a plain string
        assert_eq!(v.get("output").unwrap().as_str().unwrap(), "ok");
        Ok(())
    }

    #[test]
    fn serializes_failure_as_string() -> Result<()> {
        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call1".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("bad".into()),
                success: Some(false),
            },
        };

        let json = serde_json::to_string(&item)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;

        assert_eq!(v.get("output").unwrap().as_str().unwrap(), "bad");
        Ok(())
    }

    #[test]
    fn converts_unstructured_mcp_content_to_items() {
        let content = vec![
            serde_json::json!({"type":"text","text":"caption"}),
            serde_json::json!({
                "type": "resource_link",
                "uri": "file:///notes.txt",
                "name": "notes",
            }),
            serde_json::json!({
                "type": "audio",
                "mimeType": "audio/wav",
            }),
        ];
        let call_tool_result = CallToolResult {
            content: content.clone(),
            structured_content: Some(serde_json::Value::Null),
            is_error: Some(false),
            meta: None,
        };

        let resource_link = serde_json::to_string(&content[1]).expect("serialize resource link");
        let malformed_audio =
            serde_json::to_string(&content[2]).expect("serialize malformed audio");
        assert_eq!(
            call_tool_result.as_function_call_output_payload(),
            FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "caption".to_string(),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: resource_link,
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: malformed_audio,
                    },
                ]),
                success: Some(true),
            }
        );
    }

    #[test]
    fn preserves_structured_mcp_content() {
        let call_tool_result = CallToolResult {
            content: vec![serde_json::json!({"type":"text","text":"ignored"})],
            structured_content: Some(serde_json::json!({"result":"structured"})),
            is_error: Some(false),
            meta: None,
        };

        assert_eq!(
            call_tool_result.as_function_call_output_payload(),
            FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(
                    serde_json::json!({"result":"structured"}).to_string(),
                ),
                success: Some(true),
            }
        );
    }

    #[test]
    fn serializes_image_outputs_as_array() -> Result<()> {
        let call_tool_result = CallToolResult {
            content: vec![
                serde_json::json!({"type":"text","text":"caption"}),
                serde_json::json!({"type":"image","data":"BASE64","mimeType":"image/png"}),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let payload = call_tool_result.into_function_call_output_payload();
        assert_eq!(payload.success, Some(true));
        let Some(items) = payload.content_items() else {
            panic!("expected content items");
        };
        let items = items.to_vec();
        assert_eq!(
            items,
            vec![
                FunctionCallOutputContentItem::InputText {
                    text: "caption".into(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,BASE64".into(),
                    },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]
        );

        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call1".into(),
            output: payload,
        };

        let json = serde_json::to_string(&item)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;

        let output = v.get("output").expect("output field");
        assert!(output.is_array(), "expected array output");

        Ok(())
    }

    #[test]
    fn serializes_audio_outputs_as_array() -> Result<()> {
        let call_tool_result = CallToolResult {
            content: vec![
                serde_json::json!({"type":"text","text":"caption"}),
                serde_json::json!({"type":"audio","data":"BASE64","mimeType":"audio/wav"}),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let payload = call_tool_result.into_function_call_output_payload();
        assert_eq!(
            payload,
            FunctionCallOutputPayload {
                body: FunctionCallOutputBody::ContentItems(vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "caption".into(),
                    },
                    FunctionCallOutputContentItem::InputAudio {
                        audio_url: "data:audio/wav;base64,BASE64".into(),
                    },
                ]),
                success: Some(true),
            }
        );

        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call1".into(),
            output: payload,
        };

        assert_eq!(
            serde_json::to_value(item)?,
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call1",
                "output": [
                    {"type": "input_text", "text": "caption"},
                    {"type": "input_audio", "audio_url": "data:audio/wav;base64,BASE64"},
                ],
            })
        );

        Ok(())
    }

    #[test]
    fn serializes_custom_tool_image_outputs_as_array() -> Result<()> {
        let item = ResponseInputItem::CustomToolCallOutput {
            call_id: "call1".into(),
            name: None,
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: "data:image/png;base64,BASE64".into(),
                    },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                },
            ]),
        };

        let json = serde_json::to_string(&item)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;

        let output = v.get("output").expect("output field");
        assert!(output.is_array(), "expected array output");

        Ok(())
    }

    #[test]
    fn serializes_encrypted_function_output_content_as_array() -> Result<()> {
        let item = ResponseInputItem::FunctionCallOutput {
            call_id: "call1".into(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "enc_opaque".into(),
                },
            ]),
        };

        let json = serde_json::to_value(&item)?;
        assert_eq!(
            json,
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "call1",
                "output": [
                    {
                        "type": "encrypted_content",
                        "encrypted_content": "enc_opaque",
                    }
                ],
            })
        );

        Ok(())
    }

    #[test]
    fn preserves_existing_image_data_urls() -> Result<()> {
        let call_tool_result = CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "data:image/png;base64,BASE64",
                "mimeType": "image/png"
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let payload = call_tool_result.into_function_call_output_payload();
        let Some(items) = payload.content_items() else {
            panic!("expected content items");
        };
        let items = items.to_vec();
        assert_eq!(
            items,
            vec![FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,BASE64".into(),
                },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            }]
        );

        Ok(())
    }

    #[test]
    fn preserves_original_detail_metadata_on_mcp_images() -> Result<()> {
        let call_tool_result = CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "BASE64",
                "mimeType": "image/png",
                "_meta": {
                    "codex/imageDetail": "original",
                },
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let payload = call_tool_result.into_function_call_output_payload();
        let Some(items) = payload.content_items() else {
            panic!("expected content items");
        };
        let items = items.to_vec();
        assert_eq!(
            items,
            vec![FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,BASE64".into(),
                },
                detail: Some(ImageDetail::Original),
            }]
        );

        Ok(())
    }

    #[test]
    fn preserves_standard_detail_metadata_on_mcp_images() -> Result<()> {
        let call_tool_result = CallToolResult {
            content: vec![serde_json::json!({
                "type": "image",
                "data": "BASE64",
                "mimeType": "image/png",
                "_meta": {
                    "codex/imageDetail": "high",
                },
            })],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        };

        let payload = call_tool_result.into_function_call_output_payload();
        let Some(items) = payload.content_items() else {
            panic!("expected content items");
        };
        let items = items.to_vec();
        assert_eq!(
            items,
            vec![FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,BASE64".into(),
                },
                detail: Some(ImageDetail::High),
            }]
        );

        Ok(())
    }

    #[test]
    fn deserializes_array_payload_into_items() -> Result<()> {
        let json = r#"[
            {"type": "input_text", "text": "note"},
            {"type": "input_image", "image_url": "data:image/png;base64,XYZ"}
        ]"#;

        let payload: FunctionCallOutputPayload = serde_json::from_str(json)?;

        assert_eq!(payload.success, None);
        let expected_items = vec![
            FunctionCallOutputContentItem::InputText {
                text: "note".into(),
            },
            FunctionCallOutputContentItem::InputImage {
                image: ImageReference::Inline {
                    image_url: "data:image/png;base64,XYZ".into(),
                },
                detail: None,
            },
        ];
        assert_eq!(
            payload.body,
            FunctionCallOutputBody::ContentItems(expected_items.clone())
        );
        assert_eq!(
            serde_json::to_string(&payload)?,
            serde_json::to_string(&expected_items)?
        );

        Ok(())
    }

    #[test]
    fn deserializes_encrypted_array_payload_into_items() -> Result<()> {
        let json = r#"[
            {"type": "encrypted_content", "encrypted_content": "enc_opaque"}
        ]"#;

        let payload: FunctionCallOutputPayload = serde_json::from_str(json)?;
        let expected_items = vec![FunctionCallOutputContentItem::EncryptedContent {
            encrypted_content: "enc_opaque".into(),
        }];

        assert_eq!(payload.success, None);
        assert_eq!(
            payload.body,
            FunctionCallOutputBody::ContentItems(expected_items.clone())
        );
        assert_eq!(
            serde_json::to_string(&payload)?,
            serde_json::to_string(&expected_items)?
        );

        Ok(())
    }

    #[test]
    fn deserializes_compaction_alias() -> Result<()> {
        let json = r#"{"type":"compaction_summary","encrypted_content":"abc"}"#;

        let item: ResponseItem = serde_json::from_str(json)?;

        assert_eq!(
            item,
            ResponseItem::Compaction {
                id: None,
                encrypted_content: "abc".into(),
                internal_chat_message_metadata_passthrough: None,
            }
        );
        Ok(())
    }

    #[test]
    fn deserializes_context_compaction() -> Result<()> {
        let json = r#"{"type":"context_compaction","encrypted_content":"abc"}"#;

        let item: ResponseItem = serde_json::from_str(json)?;

        assert_eq!(
            item,
            ResponseItem::ContextCompaction {
                id: None,
                encrypted_content: Some("abc".into()),
                internal_chat_message_metadata_passthrough: None,
            }
        );
        Ok(())
    }

    #[test]
    fn serializes_compaction_trigger_without_payload() -> Result<()> {
        let item = ResponseItem::CompactionTrigger {};

        assert_eq!(
            serde_json::to_value(item)?,
            serde_json::json!({
                "type": "compaction_trigger",
            })
        );
        Ok(())
    }

    #[test]
    fn deserializes_compaction_trigger_without_payload() -> Result<()> {
        let json = r#"{"type":"compaction_trigger"}"#;

        let item: ResponseItem = serde_json::from_str(json)?;

        assert_eq!(item, ResponseItem::CompactionTrigger {});
        Ok(())
    }

    #[test]
    fn deserializes_legacy_ghost_snapshot_as_other() -> Result<()> {
        let json = r#"{
            "type":"ghost_snapshot",
            "ghost_commit":{
                "id":"ghost-1",
                "parent":null,
                "preexisting_untracked_files":[],
                "preexisting_untracked_dirs":[]
            }
        }"#;

        let item: ResponseItem = serde_json::from_str(json)?;

        assert_eq!(item, ResponseItem::Other);
        Ok(())
    }

    #[test]
    fn roundtrips_web_search_call_actions() -> Result<()> {
        let cases = vec![
            (
                r#"{
                    "type": "web_search_call",
                    "status": "completed",
                    "action": {
                        "type": "search",
                        "query": "weather seattle",
                        "queries": ["weather seattle", "seattle weather now"]
                    }
                }"#,
                None,
                Some(WebSearchAction::Search {
                    query: Some("weather seattle".into()),
                    queries: Some(vec!["weather seattle".into(), "seattle weather now".into()]),
                }),
                Some("completed".into()),
            ),
            (
                r#"{
                    "type": "web_search_call",
                    "status": "open",
                    "action": {
                        "type": "open_page",
                        "url": "https://example.com"
                    }
                }"#,
                None,
                Some(WebSearchAction::OpenPage {
                    url: Some("https://example.com".into()),
                }),
                Some("open".into()),
            ),
            (
                r#"{
                    "type": "web_search_call",
                    "status": "in_progress",
                    "action": {
                        "type": "find_in_page",
                        "url": "https://example.com/docs",
                        "pattern": "installation"
                    }
                }"#,
                None,
                Some(WebSearchAction::FindInPage {
                    url: Some("https://example.com/docs".into()),
                    pattern: Some("installation".into()),
                }),
                Some("in_progress".into()),
            ),
            (
                r#"{
                    "type": "web_search_call",
                    "status": "in_progress",
                    "id": "ws_partial"
                }"#,
                Some(ResponseItemId::with_suffix("ws", "partial")),
                None,
                Some("in_progress".into()),
            ),
        ];

        for (json_literal, expected_id, expected_action, expected_status) in cases {
            let parsed: ResponseItem = serde_json::from_str(json_literal)?;
            let expected = ResponseItem::WebSearchCall {
                encrypted_content: None,
                id: expected_id.clone(),
                status: expected_status.clone(),
                action: expected_action.clone(),
                internal_chat_message_metadata_passthrough: None,
            };
            assert_eq!(parsed, expected);

            let serialized = serde_json::to_value(&parsed)?;
            let expected_serialized: serde_json::Value = serde_json::from_str(json_literal)?;
            assert_eq!(serialized, expected_serialized);
        }

        Ok(())
    }

    #[test]
    fn serializes_image_user_input_without_tags() -> Result<()> {
        let image_url = "data:image/png;base64,abc".to_string();

        let item = ResponseInputItem::from(vec![UserInput::Image {
            image: ImageReference::Inline {
                image_url: image_url.clone(),
            },
            detail: None,
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                let expected = vec![ContentItem::InputImage {
                    image: ImageReference::Inline { image_url },
                    detail: Some(DEFAULT_IMAGE_DETAIL),
                }];
                assert_eq!(content, expected);
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    /// A file-backed user image must reach the Responses API without being resolved by Core.
    #[test]
    fn file_image_user_input_serializes_file_id() -> Result<()> {
        let file_id = "file_123".to_string();

        let item = ResponseInputItem::from(vec![UserInput::Image {
            image: ImageReference::File {
                file_id: file_id.clone(),
            },
            detail: None,
        }]);

        let expected = ResponseInputItem::Message {
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image: ImageReference::File { file_id },
                detail: Some(DEFAULT_IMAGE_DETAIL),
            }],
            phase: None,
        };
        assert_eq!(item, expected);
        assert_eq!(
            serde_json::to_value(item)?,
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "file_id": "file_123",
                    "detail": "high"
                }]
            })
        );

        Ok(())
    }

    #[test]
    fn serializes_audio_user_input_without_tags() -> Result<()> {
        let audio_url = "data:audio/wav;base64,abc".to_string();

        let item = ResponseInputItem::from(vec![UserInput::Audio {
            audio_url: audio_url.clone(),
        }]);

        assert_eq!(
            item,
            ResponseInputItem::Message {
                role: "user".to_string(),
                content: vec![ContentItem::InputAudio { audio_url }],
                phase: None,
            }
        );
        assert_eq!(
            serde_json::to_value(item)?,
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [
                    {
                        "type": "input_audio",
                        "audio_url": "data:audio/wav;base64,abc",
                    },
                ],
            })
        );

        Ok(())
    }

    #[test]
    fn serializes_local_audio_user_input_with_label_and_data_url() -> Result<()> {
        let temp_dir = tempdir()?;
        for (extension, mime) in [
            ("wav", "audio/wav"),
            ("mp3", "audio/mpeg"),
            ("m4a", "audio/mp4"),
            ("webm", "audio/webm"),
            ("ogg", "audio/ogg"),
        ] {
            let audio_path = temp_dir.path().join(format!("sample.{extension}"));
            std::fs::write(&audio_path, b"audio")?;

            let item = ResponseInputItem::from(vec![UserInput::LocalAudio {
                path: audio_path.clone(),
            }]);

            assert_eq!(
                item,
                ResponseInputItem::Message {
                    role: "user".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: local_audio_open_tag_text_with_path(
                                /*label_number*/ 1,
                                &audio_path,
                            ),
                        },
                        ContentItem::InputAudio {
                            audio_url: format!("data:{mime};base64,YXVkaW8="),
                        },
                        ContentItem::InputText {
                            text: audio_close_tag_text(),
                        },
                    ],
                    phase: None,
                }
            );
        }

        Ok(())
    }

    #[test]
    fn replaces_unsupported_local_audio_format_with_placeholder() -> Result<()> {
        let temp_dir = tempdir()?;
        let audio_path = temp_dir.path().join("sample.flac");
        std::fs::write(&audio_path, b"audio")?;

        let item = ResponseInputItem::from(vec![UserInput::LocalAudio {
            path: audio_path.clone(),
        }]);

        assert_eq!(
            item,
            ResponseInputItem::Message {
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!(
                        "Codex cannot attach audio at `{}`: unsupported audio format; use wav, mp3, m4a, webm, or ogg.",
                        audio_path.display()
                    ),
                }],
                phase: None,
            }
        );

        Ok(())
    }

    #[test]
    fn replaces_unreadable_local_audio_with_placeholder() {
        let audio_path = PathBuf::from("missing.wav");

        let item = ResponseInputItem::from(vec![UserInput::LocalAudio { path: audio_path }]);

        let ResponseInputItem::Message { content, .. } = item else {
            panic!("expected message response");
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected local audio error placeholder");
        };
        assert!(
            text.starts_with("Codex could not read the local audio at `missing.wav`: "),
            "unexpected placeholder: {text}"
        );
    }

    #[test]
    fn image_user_input_preserves_requested_detail() -> Result<()> {
        let image_url = "data:image/png;base64,abc".to_string();

        let item = ResponseInputItem::from(vec![UserInput::Image {
            image: ImageReference::Inline {
                image_url: image_url.clone(),
            },
            detail: Some(ImageDetail::Original),
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert_eq!(
                    content.first(),
                    Some(&ContentItem::InputImage {
                        image: ImageReference::Inline { image_url },
                        detail: Some(ImageDetail::Original),
                    })
                );
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn tool_search_call_roundtrips() -> Result<()> {
        let parsed: ResponseItem = serde_json::from_str(
            r#"{
                "type": "tool_search_call",
                "call_id": "search-1",
                "execution": "client",
                "arguments": {
                    "query": "calendar create",
                    "limit": 1
                }
            }"#,
        )?;

        assert_eq!(
            parsed,
            ResponseItem::ToolSearchCall {
                encrypted_content: None,
                id: None,
                call_id: Some("search-1".to_string()),
                status: None,
                execution: "client".to_string(),
                arguments: serde_json::json!({
                    "query": "calendar create",
                    "limit": 1,
                }),
                internal_chat_message_metadata_passthrough: None,
            }
        );

        assert_eq!(
            serde_json::to_value(&parsed)?,
            serde_json::json!({
                "type": "tool_search_call",
                "call_id": "search-1",
                "execution": "client",
                "arguments": {
                    "query": "calendar create",
                    "limit": 1,
                }
            })
        );

        Ok(())
    }

    #[test]
    fn tool_search_output_roundtrips() -> Result<()> {
        let input = ResponseInputItem::ToolSearchOutput {
            call_id: "search-1".to_string(),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: vec![serde_json::json!({
                "type": "function",
                "name": "mcp__codex_apps__calendar_create_event",
                "description": "Create a calendar event.",
                "defer_loading": true,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"}
                    },
                    "required": ["title"],
                    "additionalProperties": false,
                }
            })],
        };
        assert_eq!(
            ResponseItem::from(input.clone()),
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: Some("search-1".to_string()),
                status: "completed".to_string(),
                execution: "client".to_string(),
                tools: vec![serde_json::json!({
                    "type": "function",
                    "name": "mcp__codex_apps__calendar_create_event",
                    "description": "Create a calendar event.",
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"}
                        },
                        "required": ["title"],
                        "additionalProperties": false,
                    }
                })],
                internal_chat_message_metadata_passthrough: None,
            }
        );

        assert_eq!(
            serde_json::to_value(input)?,
            serde_json::json!({
                "type": "tool_search_output",
                "call_id": "search-1",
                "status": "completed",
                "execution": "client",
                "tools": [{
                    "type": "function",
                    "name": "mcp__codex_apps__calendar_create_event",
                    "description": "Create a calendar event.",
                    "defer_loading": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"}
                        },
                        "required": ["title"],
                        "additionalProperties": false,
                    }
                }]
            })
        );

        Ok(())
    }

    #[test]
    fn tool_search_server_items_allow_null_call_id() -> Result<()> {
        let parsed_call: ResponseItem = serde_json::from_str(
            r#"{
                "type": "tool_search_call",
                "execution": "server",
                "call_id": null,
                "status": "completed",
                "arguments": {
                    "paths": ["crm"]
                }
            }"#,
        )?;
        assert_eq!(
            parsed_call,
            ResponseItem::ToolSearchCall {
                encrypted_content: None,
                id: None,
                call_id: None,
                status: Some("completed".to_string()),
                execution: "server".to_string(),
                arguments: serde_json::json!({
                    "paths": ["crm"],
                }),
                internal_chat_message_metadata_passthrough: None,
            }
        );

        let parsed_output: ResponseItem = serde_json::from_str(
            r#"{
                "type": "tool_search_output",
                "execution": "server",
                "call_id": null,
                "status": "completed",
                "tools": []
            }"#,
        )?;
        assert_eq!(
            parsed_output,
            ResponseItem::ToolSearchOutput {
                id: None,
                call_id: None,
                status: "completed".to_string(),
                execution: "server".to_string(),
                tools: vec![],
                internal_chat_message_metadata_passthrough: None,
            }
        );

        Ok(())
    }

    /// Image associations use original input and expanded content positions, so omitted inputs,
    /// local-image framing, failed reads, and duplicate URLs cannot shift a later substitution.
    #[test]
    fn user_input_image_indices_survive_content_expansion() -> Result<()> {
        let dir = tempdir()?;
        let local_path = dir.path().join("local.png");
        std::fs::write(&local_path, TINY_PNG_BYTES)?;
        let inline_image = UserInput::Image {
            image: ImageReference::Inline {
                image_url: data_url_from_bytes("image/png", TINY_PNG_BYTES),
            },
            detail: None,
        };
        let inputs = vec![
            UserInput::Text {
                text: "compare images".to_string(),
                text_elements: Vec::new(),
            },
            UserInput::Mention {
                name: "app".to_string(),
                path: "app://example".to_string(),
            },
            UserInput::Skill {
                name: "skill".to_string(),
                path: dir.path().join("SKILL.md"),
            },
            UserInput::LocalImage {
                path: dir.path().join("missing.png"),
                detail: None,
            },
            UserInput::LocalImage {
                path: local_path,
                detail: None,
            },
            inline_image.clone(),
            inline_image,
        ];
        for preparation in [LocalImagePreparation::Defer, LocalImagePreparation::Process] {
            let mut indices = HashMap::new();
            let item =
                ResponseInputItem::from_user_input(inputs.clone(), preparation, &mut indices);
            assert_eq!(indices, HashMap::from([(4, 3), (5, 5), (6, 6)]));
            let ResponseInputItem::Message { content, .. } = item else {
                panic!("expected a user message");
            };
            for content_index in indices.into_values() {
                assert!(matches!(
                    content[content_index],
                    ContentItem::InputImage { .. }
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn mixed_remote_and_local_images_share_label_sequence() -> Result<()> {
        let image_url = "data:image/png;base64,abc".to_string();
        let dir = tempdir()?;
        let local_path = dir.path().join("local.png");
        std::fs::write(&local_path, TINY_PNG_BYTES)?;

        let item = ResponseInputItem::from(vec![
            UserInput::Image {
                image: ImageReference::Inline {
                    image_url: image_url.clone(),
                },
                detail: None,
            },
            UserInput::LocalImage {
                path: local_path.clone(),
                detail: None,
            },
        ]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert_eq!(
                    content.first(),
                    Some(&ContentItem::InputImage {
                        image: ImageReference::Inline { image_url },
                        detail: Some(DEFAULT_IMAGE_DETAIL),
                    })
                );
                assert_eq!(
                    content.get(1),
                    Some(&ContentItem::InputText {
                        text: local_image_open_tag_text_with_path(
                            /*label_number*/ 2,
                            &local_path
                        ),
                    })
                );
                assert!(matches!(
                    content.get(2),
                    Some(ContentItem::InputImage { .. })
                ));
                assert_eq!(
                    content.get(3),
                    Some(&ContentItem::InputText {
                        text: image_close_tag_text(),
                    })
                );
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn mixed_remote_and_local_audio_share_label_sequence() -> Result<()> {
        let audio_url = "data:audio/wav;base64,abc".to_string();
        let dir = tempdir()?;
        let local_path = dir.path().join("local.mp3");
        std::fs::write(&local_path, b"audio")?;

        let item = ResponseInputItem::from(vec![
            UserInput::Audio {
                audio_url: audio_url.clone(),
            },
            UserInput::LocalAudio {
                path: local_path.clone(),
            },
        ]);

        assert_eq!(
            item,
            ResponseInputItem::Message {
                role: "user".to_string(),
                content: vec![
                    ContentItem::InputAudio { audio_url },
                    ContentItem::InputText {
                        text: local_audio_open_tag_text_with_path(
                            /*label_number*/ 2,
                            &local_path,
                        ),
                    },
                    ContentItem::InputAudio {
                        audio_url: "data:audio/mpeg;base64,YXVkaW8=".to_string(),
                    },
                    ContentItem::InputText {
                        text: audio_close_tag_text(),
                    },
                ],
                phase: None,
            }
        );

        Ok(())
    }

    #[test]
    fn local_image_open_tag_preserves_path() {
        assert_eq!(
            local_image_open_tag_text_with_path(
                /*label_number*/ 1,
                std::path::Path::new(r#"/tmp/a&"<b>.png"#),
            ),
            r#"<image name=[Image #1] path="/tmp/a&"<b>.png">"#
        );
    }

    #[test]
    fn local_image_user_input_preserves_requested_detail() -> Result<()> {
        let dir = tempdir()?;
        let local_path = dir.path().join("local.png");
        std::fs::write(&local_path, TINY_PNG_BYTES)?;

        let item = ResponseInputItem::from(vec![UserInput::LocalImage {
            path: local_path,
            detail: Some(ImageDetail::Original),
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert!(matches!(
                    content.get(1),
                    Some(ContentItem::InputImage {
                        detail: Some(ImageDetail::Original),
                        ..
                    })
                ));
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn local_image_read_error_adds_placeholder() -> Result<()> {
        let dir = tempdir()?;
        let missing_path = dir.path().join("missing-image.png");

        let item = ResponseInputItem::from(vec![UserInput::LocalImage {
            path: missing_path.clone(),
            detail: None,
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentItem::InputText { text } => {
                        let display_path = missing_path.display().to_string();
                        assert!(
                            text.contains(&display_path),
                            "placeholder should mention missing path: {text}"
                        );
                        assert!(
                            text.contains("could not read"),
                            "placeholder should mention read issue: {text}"
                        );
                    }
                    other => panic!("expected placeholder text but found {other:?}"),
                }
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn local_image_non_image_adds_placeholder() -> Result<()> {
        let dir = tempdir()?;
        let json_path = dir.path().join("example.json");
        std::fs::write(&json_path, br#"{"hello":"world"}"#)?;

        let item = ResponseInputItem::from(vec![UserInput::LocalImage {
            path: json_path.clone(),
            detail: None,
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    ContentItem::InputText { text } => {
                        assert!(
                            text.contains("unsupported image `application/json`"),
                            "placeholder should mention unsupported image MIME: {text}"
                        );
                        assert!(
                            text.contains(&json_path.display().to_string()),
                            "placeholder should mention path: {text}"
                        );
                    }
                    other => panic!("expected placeholder text but found {other:?}"),
                }
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn local_image_unsupported_image_format_adds_placeholder() -> Result<()> {
        let dir = tempdir()?;
        let svg_path = dir.path().join("example.svg");
        std::fs::write(
            &svg_path,
            br#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
        )?;

        let item = ResponseInputItem::from(vec![UserInput::LocalImage {
            path: svg_path.clone(),
            detail: None,
        }]);

        match item {
            ResponseInputItem::Message { content, .. } => {
                assert_eq!(content.len(), 1);
                let expected = format!(
                    "Codex cannot attach image at `{}`: unsupported image `image/svg+xml`.",
                    svg_path.display()
                );
                match &content[0] {
                    ContentItem::InputText { text } => assert_eq!(text, &expected),
                    other => panic!("expected placeholder text but found {other:?}"),
                }
            }
            other => panic!("expected message response but got {other:?}"),
        }

        Ok(())
    }
}

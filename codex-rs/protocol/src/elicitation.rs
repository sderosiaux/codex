//! MCP elicitation requests surfaced to clients and their responses.

use crate::mcp::RequestId;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use ts_rs::TS;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[ts(tag = "mode")]
pub enum ElicitationRequest {
    Form {
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        requested_schema: JsonValue,
    },
    #[serde(rename = "openai/form")]
    #[ts(rename = "openai/form")]
    OpenAiForm {
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        requested_schema: JsonValue,
    },
    #[serde(rename = "openaiForm")]
    #[ts(rename = "openaiForm")]
    OpenAiElicitationForm {
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        requested_schema: JsonValue,
    },
    Url {
        #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, rename = "_meta")]
        meta: Option<JsonValue>,
        message: String,
        url: String,
        elicitation_id: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ElicitationAbandonedEvent {
    pub server_name: String,
    /// Original Core-generated response token of the unanswered request.
    #[ts(type = "string | number")]
    pub id: RequestId,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ElicitationRequestEvent {
    /// Turn ID that this elicitation belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    pub server_name: String,
    #[ts(type = "string | number")]
    pub id: RequestId,
    pub request: ElicitationRequest,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

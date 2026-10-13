use super::CodexErrorInfo;
use super::ThreadItem;
use super::TurnStatus;
use crate::JsonSchema;
use crate::TS;
use codex_protocol::protocol::MisalignmentErrorDetails as CoreMisalignmentErrorDetails;
use codex_protocol::protocol::MisalignmentSteer as CoreMisalignmentSteer;
use codex_protocol::protocol::SessionSource as CoreSessionSource;
use codex_protocol::protocol::SubAgentSource as CoreSubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode as CoreThreadHistoryMode;
use codex_protocol::protocol::ThreadSource as CoreThreadSource;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use thiserror::Error;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
#[derive(Default)]
pub enum SessionSource {
    Cli,
    #[serde(rename = "vscode")]
    #[ts(rename = "vscode")]
    #[default]
    VsCode,
    Exec,
    AppServer,
    Custom(String),
    SubAgent(CoreSubAgentSource),
    #[serde(other)]
    Unknown,
}

impl From<CoreSessionSource> for SessionSource {
    fn from(value: CoreSessionSource) -> Self {
        match value {
            CoreSessionSource::Cli => SessionSource::Cli,
            CoreSessionSource::VSCode => SessionSource::VsCode,
            CoreSessionSource::Exec => SessionSource::Exec,
            CoreSessionSource::Mcp => SessionSource::AppServer,
            CoreSessionSource::Custom(source) => SessionSource::Custom(source),
            // We do not want to render those at the app-server level.
            CoreSessionSource::Internal(_) => SessionSource::Unknown,
            CoreSessionSource::SubAgent(sub) => SessionSource::SubAgent(sub),
            CoreSessionSource::Unknown => SessionSource::Unknown,
        }
    }
}

impl From<SessionSource> for CoreSessionSource {
    fn from(value: SessionSource) -> Self {
        match value {
            SessionSource::Cli => CoreSessionSource::Cli,
            SessionSource::VsCode => CoreSessionSource::VSCode,
            SessionSource::Exec => CoreSessionSource::Exec,
            SessionSource::AppServer => CoreSessionSource::Mcp,
            SessionSource::Custom(source) => CoreSessionSource::Custom(source),
            SessionSource::SubAgent(sub) => CoreSessionSource::SubAgent(sub),
            SessionSource::Unknown => CoreSessionSource::Unknown,
        }
    }
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum ThreadHistoryMode {
    #[default]
    Legacy,
    Paginated,
}

impl From<CoreThreadHistoryMode> for ThreadHistoryMode {
    fn from(value: CoreThreadHistoryMode) -> Self {
        match value {
            CoreThreadHistoryMode::Legacy => Self::Legacy,
            CoreThreadHistoryMode::Paginated => Self::Paginated,
        }
    }
}

impl From<ThreadHistoryMode> for CoreThreadHistoryMode {
    fn from(value: ThreadHistoryMode) -> Self {
        match value {
            ThreadHistoryMode::Legacy => Self::Legacy,
            ThreadHistoryMode::Paginated => Self::Paginated,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
#[ts(export_to = "v2/")]
pub enum ThreadSource {
    User,
    Subagent,
    Feature(String),
    MemoryConsolidation,
}

impl TryFrom<String> for ThreadSource {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse::<CoreThreadSource>().map(Into::into)
    }
}

impl From<ThreadSource> for String {
    fn from(value: ThreadSource) -> Self {
        CoreThreadSource::from(value).into()
    }
}

impl From<CoreThreadSource> for ThreadSource {
    fn from(value: CoreThreadSource) -> Self {
        match value {
            CoreThreadSource::User => ThreadSource::User,
            CoreThreadSource::Subagent => ThreadSource::Subagent,
            CoreThreadSource::Feature(feature) => ThreadSource::Feature(feature),
            CoreThreadSource::MemoryConsolidation => ThreadSource::MemoryConsolidation,
        }
    }
}

impl From<ThreadSource> for CoreThreadSource {
    fn from(value: ThreadSource) -> Self {
        match value {
            ThreadSource::User => CoreThreadSource::User,
            ThreadSource::Subagent => CoreThreadSource::Subagent,
            ThreadSource::Feature(feature) => CoreThreadSource::Feature(feature),
            ThreadSource::MemoryConsolidation => CoreThreadSource::MemoryConsolidation,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct GitInfo {
    pub sha: Option<String>,
    pub branch: Option<String>,
    pub origin_url: Option<String>,
}

/// An independently persisted, user-visible thread section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSection {
    /// Opaque UUIDv7 identity that remains stable when the section is renamed.
    pub id: String,
    /// The current user-visible section name.
    pub name: String,
    /// Optional appearance synchronized across clients.
    #[serde(default)]
    pub appearance: Option<ThreadSectionAppearance>,
}

/// Extensible visual presentation for a custom thread section.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadSectionAppearance {
    pub icon: Option<String>,
    pub color: Option<String>,
}

// TODO: Remove this compatibility decoder after app-server versions that omitted
// `projectId` have aged out of the supported TUI -> remote app-server version-skew window.

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct Turn {
    /// Identifier for this turn. Codex-generated turn IDs are UUIDv7.
    pub id: String,
    /// ID of the first turn in the chain of work that led to this turn.
    /// Pass this as `rootTurnId` when starting work on behalf of this turn.
    /// May be null in older history or a `review/start` response.
    pub root_turn_id: Option<String>,
    /// Thread items currently included in this turn payload.
    pub items: Vec<ThreadItem>,
    /// Describes how much of `items` has been loaded for this turn.
    #[serde(default)]
    pub items_view: TurnItemsView,
    pub status: TurnStatus,
    /// Error associated with a failed or interrupted turn.
    pub error: Option<TurnError>,
    /// Unix timestamp (in seconds) when the turn started.
    #[ts(type = "number | null")]
    pub started_at: Option<i64>,
    /// Unix timestamp (in seconds) when the turn completed.
    #[ts(type = "number | null")]
    pub completed_at: Option<i64>,
    /// Duration between turn start and completion in milliseconds, if known.
    #[ts(type = "number | null")]
    pub duration_ms: Option<i64>,
}

#[derive(Default, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum TurnItemsView {
    /// `items` was not loaded for this turn. The field is intentionally empty.
    NotLoaded,
    /// `items` contains only a display summary for this turn.
    Summary,
    /// `items` contains every ThreadItem available from persisted app-server history for this turn.
    #[default]
    Full,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Error)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
#[error("{message}")]
pub struct TurnError {
    pub message: String,
    pub codex_error_info: Option<CodexErrorInfo>,
    #[serde(default)]
    pub additional_details: Option<String>,
    /// Optional public explanation and continuation instruction for a misalignment block.
    #[serde(default)]
    pub misalignment: Option<MisalignmentErrorDetails>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MisalignmentErrorDetails {
    /// Open-ended classification; clients must accept categories added by Responses.
    pub error_type: Option<String>,
    /// A substantive localized explanation is required before offering continuation.
    pub detailed_explanation: Option<String>,
    /// Instruction to submit as the next turn's user input if continuation is confirmed.
    pub steer: Option<MisalignmentSteer>,
    /// Opaque server-issued block target. Presence alone does not enable target-based continuation.
    #[ts(optional = nullable)]
    pub review_target: Option<String>,
}

impl fmt::Debug for MisalignmentErrorDetails {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MisalignmentErrorDetails")
            .field("error_type", &self.error_type)
            .field(
                "has_detailed_explanation",
                &self.detailed_explanation.is_some(),
            )
            .field("has_steer", &self.steer.is_some())
            .field("has_review_target", &self.review_target.is_some())
            .finish()
    }
}

impl From<CoreMisalignmentErrorDetails> for MisalignmentErrorDetails {
    fn from(value: CoreMisalignmentErrorDetails) -> Self {
        Self {
            error_type: value.error_type,
            detailed_explanation: value.detailed_explanation,
            steer: value.steer.map(Into::into),
            review_target: value.review_target,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct MisalignmentSteer {
    pub message: String,
}

impl fmt::Debug for MisalignmentSteer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MisalignmentSteer")
            .field("message", &"[REDACTED]")
            .finish()
    }
}

impl From<CoreMisalignmentSteer> for MisalignmentSteer {
    fn from(value: CoreMisalignmentSteer) -> Self {
        Self {
            message: value.message,
        }
    }
}

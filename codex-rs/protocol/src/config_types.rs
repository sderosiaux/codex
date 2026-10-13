use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_redacted_string::RedactedString;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;
use strum_macros::Display;
use strum_macros::EnumIter;
use ts_rs::TS;
use wildmatch::WildMatchPattern;

use crate::openai_models::ReasoningEffort;

/// Limit for the text included in `codex.tool_result` log records.
/// This does not affect model-visible output. Raising it can expose more tool data to logs.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(default)]
pub struct ToolResultLogConfig {
    /// Maximum UTF-8 bytes before the truncation notice. Defaults to 2048.
    pub max_bytes: usize,
}

impl Default for ToolResultLogConfig {
    fn default() -> Self {
        Self {
            max_bytes: 2 * 1024,
        }
    }
}

/// Selects which part of the active context is charged against
/// `model_auto_compact_token_limit`.
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutoCompactTokenLimitScope {
    /// Count the full active context against the limit.
    #[default]
    Total,
    /// Count sampled output and later growth after the carried window prefix.
    BodyAfterPrefix,
}

/// A summary of the reasoning performed by the model. This can be useful for
/// debugging and understanding the model's reasoning process.
/// See https://platform.openai.com/docs/guides/reasoning?api-mode=responses#reasoning-summaries
#[derive(
    Debug, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ReasoningSummary {
    #[default]
    Auto,
    Concise,
    Detailed,
    /// Option to disable reasoning summaries.
    None,
}

/// Controls output length/detail on GPT-5 models via the Responses API.
/// Serialized with lowercase values to match the OpenAI API.
#[derive(
    Hash,
    Debug,
    Serialize,
    Deserialize,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Verbosity {
    Low,
    #[default]
    Medium,
    High,
}

/// Validated plain profile-v2 name used to select `$CODEX_HOME/<name>.config.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileV2Name(String);

impl ProfileV2Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProfileV2NameParseError {
    value: String,
}

impl fmt::Display for ProfileV2NameParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid --profile value `{}`; pass a plain name such as `work`",
            self.value
        )
    }
}

impl std::error::Error for ProfileV2NameParseError {}

impl FromStr for ProfileV2Name {
    type Err = ProfileV2NameParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ProfileV2NameParseError {
                value: value.to_string(),
            });
        }

        Ok(Self(value.to_string()))
    }
}

impl Deref for ProfileV2Name {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for ProfileV2Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ShellEnvironmentPolicyInherit {
    /// "Core" environment variables for the platform. On UNIX, this would
    /// include HOME, LOGNAME, PATH, SHELL, and USER, among others.
    Core,

    /// Inherits the full environment from the parent process.
    #[default]
    All,

    /// Do not inherit any environment variables from the parent process.
    None,
}

/// Assigns a shell environment variable pattern to the include-only or exclude
/// set. Includes do not re-add variables removed by another exclude pattern.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export_to = "v2/")]
pub enum ShellEnvironmentPolicyFilter {
    Include,
    Exclude,
}

pub type EnvironmentVariablePattern = WildMatchPattern<'*', '?'>;

/// Deriving the `env` based on this policy works as follows:
/// 1. Create an initial map based on the `inherit` policy.
/// 2. If `ignore_default_excludes` is false, filter the map using the default
///    exclude pattern(s), which are: `"*KEY*"`, `"*SECRET*"`, and `"*TOKEN*"`.
/// 3. If `exclude` is not empty, filter the map using the provided patterns.
/// 4. Insert any entries from `r#set` into the map.
/// 5. If non-empty, filter the map using the `include_only` patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellEnvironmentPolicy {
    /// Starting point when building the environment.
    pub inherit: ShellEnvironmentPolicyInherit,

    /// True to skip the check to exclude default environment variables that
    /// contain "KEY", "SECRET", or "TOKEN" in their name. Defaults to true.
    pub ignore_default_excludes: bool,

    /// Environment variable names to exclude from the environment.
    pub exclude: Vec<EnvironmentVariablePattern>,

    /// (key, value) pairs to insert in the environment.
    pub r#set: HashMap<String, String>,

    /// Environment variable names to retain in the environment.
    pub include_only: Vec<EnvironmentVariablePattern>,

    /// If true, the shell profile will be used to run the command.
    pub use_profile: bool,
}

impl Default for ShellEnvironmentPolicy {
    fn default() -> Self {
        Self {
            inherit: ShellEnvironmentPolicyInherit::All,
            ignore_default_excludes: true,
            exclude: Vec::new(),
            r#set: HashMap::new(),
            include_only: Vec::new(),
            use_profile: false,
        }
    }
}

/// Deprecated: `friendly` and `pragmatic` no longer select a style.
#[derive(
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Display,
    JsonSchema,
    TS,
    PartialOrd,
    Ord,
    EnumIter,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Personality {
    None,
    Friendly,
    Pragmatic,
}

/// Controls the effective multi-agent delegation instructions for a turn. `custom` means the
/// configured mode hint defines the policy instead of a built-in policy.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Display, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase", from = "MultiAgentModeWire")]
#[ts(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum MultiAgentMode {
    Custom(String),
    #[default]
    ExplicitRequestOnly,
    Proactive,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum MultiAgentModeWire {
    None,
    Custom(String),
    ExplicitRequestOnly,
    Proactive,
}

impl From<MultiAgentModeWire> for MultiAgentMode {
    fn from(value: MultiAgentModeWire) -> Self {
        match value {
            MultiAgentModeWire::None => Self::Custom(String::new()),
            MultiAgentModeWire::Custom(hint_text) => Self::Custom(hint_text),
            MultiAgentModeWire::ExplicitRequestOnly => Self::ExplicitRequestOnly,
            MultiAgentModeWire::Proactive => Self::Proactive,
        }
    }
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum WebSearchMode {
    Disabled,
    #[default]
    Cached,
    Indexed,
    Live,
}

impl WebSearchMode {
    /// Restricts search to the access permitted by both modes.
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Disabled, _) | (_, Self::Disabled) => Self::Disabled,
            (Self::Cached, _) | (_, Self::Cached) => Self::Cached,
            (Self::Indexed, _) | (_, Self::Indexed) => Self::Indexed,
            (Self::Live, Self::Live) => Self::Live,
        }
    }
}

/// A model-facing surface on which a tool can be exposed.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ToolExposureSurface {
    /// Tools discovered later through tool search.
    Deferred,
    /// Tools present in the model's initial tool list.
    Direct,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchLocation {
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

impl WebSearchLocation {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            country: other.country.clone().or_else(|| self.country.clone()),
            region: other.region.clone().or_else(|| self.region.clone()),
            city: other.city.clone().or_else(|| self.city.clone()),
            timezone: other.timezone.clone().or_else(|| self.timezone.clone()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchToolConfig {
    pub context_size: Option<WebSearchContextSize>,
    pub allowed_domains: Option<Vec<String>>,
    pub location: Option<WebSearchLocation>,
}

impl WebSearchToolConfig {
    pub fn merge(&self, other: &Self) -> Self {
        Self {
            context_size: other.context_size.or(self.context_size),
            allowed_domains: other
                .allowed_domains
                .clone()
                .or_else(|| self.allowed_domains.clone()),
            location: match (&self.location, &other.location) {
                (Some(location), Some(other_location)) => Some(location.merge(other_location)),
                (Some(location), None) => Some(location.clone()),
                (None, Some(other_location)) => Some(other_location.clone()),
                (None, None) => None,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchFilters {
    pub allowed_domains: Option<Vec<String>>,
}

#[derive(
    Debug, Serialize, Deserialize, Clone, Copy, Default, PartialEq, Eq, Display, JsonSchema, TS,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum WebSearchUserLocationType {
    #[default]
    Approximate,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchUserLocation {
    #[serde(default)]
    pub r#type: WebSearchUserLocationType,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchConfig {
    pub filters: Option<WebSearchFilters>,
    pub user_location: Option<WebSearchUserLocation>,
    pub search_context_size: Option<WebSearchContextSize>,
}

impl From<WebSearchLocation> for WebSearchUserLocation {
    fn from(location: WebSearchLocation) -> Self {
        Self {
            r#type: WebSearchUserLocationType::Approximate,
            country: location.country,
            region: location.region,
            city: location.city,
            timezone: location.timezone,
        }
    }
}

impl From<WebSearchToolConfig> for WebSearchConfig {
    fn from(config: WebSearchToolConfig) -> Self {
        Self {
            filters: config
                .allowed_domains
                .map(|allowed_domains| WebSearchFilters {
                    allowed_domains: Some(allowed_domains),
                }),
            user_location: config.location.map(Into::into),
            search_context_size: config.context_size,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}

/// Request/config sentinel for explicit standard routing.
///
/// This is not a catalog service tier id. It means the user intentionally
/// selected no service tier, so model catalog defaults should not apply.
pub const SERVICE_TIER_DEFAULT_REQUEST_VALUE: &str = "default";

impl ServiceTier {
    pub const fn request_value(self) -> &'static str {
        match self {
            Self::Fast => "priority",
            Self::Flex => "flex",
        }
    }

    pub fn from_request_value(value: &str) -> Option<Self> {
        match value {
            "fast" | "priority" => Some(Self::Fast),
            "flex" => Some(Self::Flex),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ForcedLoginMethod {
    Chatgpt,
    Api,
}

const DEFAULT_PROVIDER_AUTH_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS: u64 = 300_000;

/// Configuration for obtaining a provider bearer token from a command.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelProviderAuthInfo {
    /// Command to execute. Bare names are resolved via `PATH`; paths are resolved against `cwd`.
    pub command: String,

    /// Command arguments.
    #[serde(default)]
    pub args: Vec<RedactedString>,

    /// Maximum time to wait for the token command to exit successfully.
    #[serde(default = "default_provider_auth_timeout_ms")]
    pub timeout_ms: NonZeroU64,

    /// Maximum age for the cached token before rerunning the command.
    /// Set to `0` to disable proactive refresh and only rerun after a 401 retry path.
    #[serde(default = "default_provider_auth_refresh_interval_ms")]
    pub refresh_interval_ms: u64,

    /// Working directory used when running the token command.
    #[serde(default = "default_provider_auth_cwd")]
    #[schemars(skip_serializing_if = "is_default_provider_auth_cwd")]
    pub cwd: AbsolutePathBuf,
}

impl ModelProviderAuthInfo {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.get())
    }

    pub fn refresh_interval(&self) -> Option<Duration> {
        NonZeroU64::new(self.refresh_interval_ms).map(|value| Duration::from_millis(value.get()))
    }
}

fn default_provider_auth_timeout_ms() -> NonZeroU64 {
    non_zero_u64(
        DEFAULT_PROVIDER_AUTH_TIMEOUT_MS,
        "model_providers.<id>.auth.timeout_ms",
    )
}

fn default_provider_auth_refresh_interval_ms() -> u64 {
    DEFAULT_PROVIDER_AUTH_REFRESH_INTERVAL_MS
}

fn non_zero_u64(value: u64, field_name: &str) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(value) => value,
        None => panic!("{field_name} must be non-zero"),
    }
}

fn default_provider_auth_cwd() -> AbsolutePathBuf {
    let deserializer = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(".");
    if let Ok(cwd) = AbsolutePathBuf::deserialize(deserializer) {
        return cwd;
    }

    match AbsolutePathBuf::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => panic!("provider auth cwd must resolve: {err}"),
    }
}

fn is_default_provider_auth_cwd(path: &AbsolutePathBuf) -> bool {
    path == &default_provider_auth_cwd()
}

/// Initial collaboration mode to use when the TUI starts.
#[derive(
    Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    Plan,
    #[default]
    #[serde(
        alias = "code",
        alias = "pair_programming",
        alias = "execute",
        alias = "custom"
    )]
    Default,
}

pub const TUI_VISIBLE_COLLABORATION_MODES: [ModeKind; 2] = [ModeKind::Default, ModeKind::Plan];

impl ModeKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Default => "Default",
        }
    }

    pub const fn is_tui_visible(self) -> bool {
        matches!(self, Self::Plan | Self::Default)
    }

    pub const fn allows_request_user_input(self) -> bool {
        matches!(self, Self::Plan)
    }
}

/// Collaboration mode for a Codex session.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub struct CollaborationMode {
    pub mode: ModeKind,
    pub settings: Settings,
}

impl CollaborationMode {
    /// Returns a reference to the settings.
    fn settings_ref(&self) -> &Settings {
        &self.settings
    }

    pub fn model(&self) -> &str {
        self.settings_ref().model.as_str()
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.settings_ref().reasoning_effort.clone()
    }

    /// Updates the collaboration mode with new model and/or effort values.
    ///
    /// - `model`: `Some(s)` to update the model, `None` to keep the current model
    /// - `effort`: `Some(Some(e))` to set effort to `e`, `Some(None)` to clear effort, `None` to keep current effort
    /// - `developer_instructions`: `Some(Some(s))` to set instructions, `Some(None)` to clear them, `None` to keep current
    ///
    /// Returns a new `CollaborationMode` with updated values, preserving the mode.
    pub fn with_updates(
        &self,
        model: Option<String>,
        effort: Option<Option<ReasoningEffort>>,
        developer_instructions: Option<Option<String>>,
    ) -> Self {
        let settings = self.settings_ref();
        let updated_settings = Settings {
            model: model.unwrap_or_else(|| settings.model.clone()),
            reasoning_effort: effort.unwrap_or_else(|| settings.reasoning_effort.clone()),
            developer_instructions: developer_instructions
                .unwrap_or_else(|| settings.developer_instructions.clone()),
        };

        CollaborationMode {
            mode: self.mode,
            settings: updated_settings,
        }
    }

    /// Applies a mask to this collaboration mode, returning a new collaboration mode
    /// with the mask values applied. Fields in the mask that are `Some` will override
    /// the corresponding fields, while `None` values will preserve the original values.
    ///
    /// The `name` field in the mask is ignored as it's metadata for the mask itself.
    pub fn apply_mask(&self, mask: &CollaborationModeMask) -> Self {
        let settings = self.settings_ref();
        CollaborationMode {
            mode: mask.mode.unwrap_or(self.mode),
            settings: Settings {
                model: mask.model.clone().unwrap_or_else(|| settings.model.clone()),
                reasoning_effort: mask
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| settings.reasoning_effort.clone()),
                developer_instructions: mask
                    .developer_instructions
                    .clone()
                    .unwrap_or_else(|| settings.developer_instructions.clone()),
            },
        }
    }
}

/// Settings for a collaboration mode.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct Settings {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub developer_instructions: Option<String>,
}

/// A mask for collaboration mode settings, allowing partial updates.
/// All fields except `name` are optional, enabling selective updates.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema, TS)]
pub struct CollaborationModeMask {
    pub name: String,
    pub mode: Option<ModeKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<Option<ReasoningEffort>>,
    pub developer_instructions: Option<Option<String>>,
}

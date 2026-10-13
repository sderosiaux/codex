use crate::FeatureConfig;
use codex_utils_redacted_string::RedactedString;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Connection to a board provisioned by the research host.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoteMessageBoardConfigToml {
    pub url: String,
    /// Board credential supplied directly by a runtime config override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<RedactedString>,
    /// Read the credential from this environment variable instead of bearer_token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolRegistryConfigToml {
    /// Fail the turn when multiple tools share the same effective name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_on_tool_collisions: Option<bool>,
    /// Include authoritative tool information in per-turn request metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_metadata_includes_tool_info: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NonPrefixedMcpToolNamesConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// MCP servers whose tools should omit the legacy `mcp__` namespace prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_names: Option<Vec<String>>,
}

impl FeatureConfig for NonPrefixedMcpToolNamesConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MultiAgentV2ConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_concurrent_threads_per_session: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 3600000))]
    pub min_wait_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 3600000))]
    pub max_wait_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 3600000))]
    pub default_wait_timeout_ms: Option<i64>,
    /// Deprecated compatibility field. Its value is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_hint_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_agent_usage_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_usage_hint_text: Option<String>,
    /// Overrides inherited developer instructions for subagents without role-specific instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_developer_instructions: Option<String>,
    /// Preserve the parent's model-visible history on full forks and append child instructions.
    /// Defaults to false. Legacy compaction without a reusable context baseline still rebuilds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserve_fork_prefix: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_agent_mode_hint_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-zA-Z0-9_-]+$"))]
    pub tool_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hide_spawn_agent_metadata: Option<bool>,
    /// Exposes `model` and `reasoning_effort` on the multi-agent v2 spawn tool and adds
    /// corresponding guidance to root and subagent usage hints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expose_spawn_agent_model_overrides: Option<bool>,
    /// Expose the multi-agent v2 `wait_agent` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_agent_enabled: Option<bool>,
    /// Disable the model's direct-message tools; spawning and automatic child results remain available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_direct_message: Option<bool>,
    /// Use a session-scoped remote board instead of the in-memory board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_board_remote: Option<RemoteMessageBoardConfigToml>,
}

impl FeatureConfig for MultiAgentV2ConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextManagementConfigToml {
    /// Enables experimental context management.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experimental_mode: Option<bool>,
}

impl FeatureConfig for ContextManagementConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.experimental_mode
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenBudgetConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Whether to expose the built-in history and notes extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_history_notes_extension: Option<bool>,
    /// Number of tokens remaining before auto-compaction when the wrap-up reminder is emitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub reminder_threshold_tokens: Option<i64>,
    /// Reminder template. `{n_remaining}` is replaced with the tokens remaining before
    /// auto-compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 2000))]
    pub reminder_message_template: Option<String>,
    /// Guidance appended to the context-window metadata in a developer message.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 2000))]
    pub guidance_message: Option<String>,
    /// Developer message sampled before an automatic context-window rollover.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 2000))]
    pub auto_compact_fallback_prompt: Option<String>,
    /// Additional tokens available after the compaction threshold for fallback note-taking.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub auto_compact_fallback_buffer_tokens: Option<i64>,
}

impl FeatureConfig for TokenBudgetConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RolloutBudgetConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub limit_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Remaining weighted-token values that trigger reminders when crossed.
    pub reminder_at_remaining_tokens: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub sampling_token_weight: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0))]
    pub prefill_token_weight: Option<f64>,
}

impl FeatureConfig for RolloutBudgetConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrentTimeSource {
    #[default]
    System,
    External,
}

/// Which inference boundaries may receive current-time reminders.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CurrentTimeReminderDeliveryMode {
    /// Allow a reminder before any inference request once the interval is due.
    #[default]
    AnyInference,
    /// Allow reminders after user input or tool output; new context windows still force one.
    AfterUserOrToolOutput,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CurrentTimeReminderConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reminder_interval_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_source: Option<CurrentTimeSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<CurrentTimeReminderDeliveryMode>,
    /// Expose the input-interruptible `clock.sleep` tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sleep_tool: Option<bool>,
}

impl FeatureConfig for CurrentTimeReminderConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

/// How the sleep tool is selected when its feature gate is enabled.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SleepToolMode {
    /// Preserve the existing model and legacy clock configuration defaults.
    #[default]
    ModelDriven,
    /// Register sleep regardless of the model or legacy clock configuration.
    AlwaysOn,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SleepToolConfigToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SleepToolMode>,
}

impl FeatureConfig for SleepToolConfigToml {
    fn enabled(&self) -> Option<bool> {
        self.enabled
    }
}

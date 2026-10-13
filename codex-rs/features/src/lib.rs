//! Centralized feature flags and metadata.
//!
//! This crate defines the feature registry plus the logic used to resolve an
//! effective feature set from config-like inputs.

use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use toml::Table;

mod feature_configs;
mod legacy;
pub use feature_configs::ContextManagementConfigToml;
pub use feature_configs::CurrentTimeReminderConfigToml;
pub use feature_configs::CurrentTimeReminderDeliveryMode;
pub use feature_configs::CurrentTimeSource;
pub use feature_configs::MultiAgentV2ConfigToml;
pub use feature_configs::NonPrefixedMcpToolNamesConfigToml;
pub use feature_configs::RemoteMessageBoardConfigToml;
pub use feature_configs::RolloutBudgetConfigToml;
pub use feature_configs::SleepToolConfigToml;
pub use feature_configs::SleepToolMode;
pub use feature_configs::TokenBudgetConfigToml;
pub use feature_configs::ToolRegistryConfigToml;
pub use legacy::legacy_feature_keys;

/// High-level lifecycle stage for a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Features that are still under development, not ready for external use
    UnderDevelopment,
    /// Experimental features made available to users through the `/experimental` menu
    Experimental {
        name: &'static str,
        menu_description: &'static str,
        announcement: &'static str,
    },
    /// Stable features. The feature flag is kept for ad-hoc enabling/disabling
    Stable,
    /// Deprecated feature that should not be used anymore.
    Deprecated,
    /// The feature flag is useless but kept for backward compatibility reason.
    Removed,
}

impl Stage {
    pub fn experimental_menu_name(self) -> Option<&'static str> {
        match self {
            Stage::Experimental { name, .. } => Some(name),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Removed => None,
        }
    }

    pub fn experimental_menu_description(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                menu_description, ..
            } => Some(menu_description),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Removed => None,
        }
    }

    pub fn experimental_announcement(self) -> Option<&'static str> {
        match self {
            Stage::Experimental {
                announcement: "", ..
            } => None,
            Stage::Experimental { announcement, .. } => Some(announcement),
            Stage::UnderDevelopment | Stage::Stable | Stage::Deprecated | Stage::Removed => None,
        }
    }
}

/// Unique features toggled via configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Discover model catalogs for OpenAI API-key authentication.
    ApiKeyModelDiscovery,
    /// Forward explicit programs with builtin OpenAI API keys.
    ApiKeyCyberAccessPrograms,
    // Stable.
    /// Enable the default shell tool.
    ShellTool,
    /// Enable the built-in local image viewer.
    ViewImage,
    /// Allow registration of the built-in sleep tool.
    SleepTool,
    /// Enable Claude-style lifecycle hooks loaded from hooks.json files.
    CodexHooks,
    /// Store CLI auth in the encrypted local secrets backend when keyring storage is selected.
    SecretAuthStorage,

    // Experimental
    /// Send per-content-entry classifications in internal Responses metadata.
    ContentItemKinds,
    /// Record model-attempted tool calls in internal Responses metadata.
    ExecutedToolCallMetadata,
    /// Allow unified exec commands to allocate an interactive terminal.
    UnifiedExecTty,
    /// Stream structured progress while apply_patch input is being generated.
    ApplyPatchStreamingEvents,
    /// Allow the model to request web searches that fetch live content.
    WebSearchRequest,
    /// Allow the model to request web searches that fetch cached content.
    /// Takes precedence over `WebSearchRequest`.
    WebSearchCached,
    /// Expose the extension-backed standalone web search tool.
    StandaloneWebSearch,
    /// Experimental shell snapshotting.
    ShellSnapshot,
    /// Restore bundled tools to PATH after Codex's login shell starts.
    LoginShellPackagePath,
    /// Expose the selected PowerShell execution host's bounded major/minor version.
    PowerShellShellVersion,
    /// Keep policy-filtered shell snapshots entirely in executor memory.
    ShellSnapshotV2,
    /// Use the current working directory for turn diff display paths.
    CwdRelativeTurnDiffs,
    /// Enable runtime metrics snapshots via a manual reader.
    RuntimeMetrics,
    /// Compress request bodies (zstd) when sending streaming requests to codex-backend.
    EnableRequestCompression,
    /// Keep active sampling turns alive until a failed network connection recovers.
    UnboundedConnectionRetries,
    /// Respect host system proxy settings for Codex-owned network clients.
    RespectSystemProxy,
    /// Retry eligible bootstrap requests through the system proxy after normal routing fails.
    SystemProxyFallback,
    /// Enable collab tools.
    Collab,
    /// Enable task-path-based multi-agent routing.
    MultiAgentV2,
    /// Keep spawn model choices in append-only context instead of tool descriptions.
    ModelCatalogInContext,
    /// Inherit client-defined dynamic tools in fresh V2 subagents.
    MultiAgentV2DynamicTools,
    /// Use each subagent model's default context window and auto-compaction threshold.
    SubagentDefaultContextLimits,
    /// Keep sampling through reasoning and commentary boundaries when agent mail arrives.
    /// Pending mail is delivered at the next normal input boundary instead.
    DeferMailboxPreemption,
    /// Preempt responses and yield foreground code-mode observations on new user input.
    InstantInterrupt,
    /// Enable shared discussion tools for an agent tree.
    AgentMessageBoard,
    /// Route first-party ChatGPT requests through PSP.
    Psp,
    /// Enable MCP protocol version 2026-07-28 support.
    Mcp20260728,
    /// Let RMCP coordinate OAuth refresh through the configured credential store.
    McpOAuthRefreshCoordination,
    /// Enable enterprise refresh-token authorization for configured MCP resources.
    UseXaa,
    /// Describe deferred tool namespaces in the model-visible world state.
    DeferredToolWorldState,
    /// Track top-level tool definitions in world state and emit incremental context updates.
    IncrementalTools,
    /// Expose MCP model-visible namespaces without the legacy `mcp__` prefix.
    NonPrefixedMcpToolNames,
    /// Skip host skill snapshots when no registered contributor requires them.
    SkipHostSkillDiscovery,
    /// Enable extension-backed image generation.
    ImageGeneration,
    /// Tell the model when a prompt image was resized and include its dimensions.
    ImageResizeNotice,
    /// Apply one shared pixel and token budget to every image, regardless of legacy detail hints.
    UnifiedImageBudget,
    /// Request sequential cutoff reasoning summary delivery.
    ConcurrentReasoningSummaries,
    /// Request encrypted sampled output for token-preserving replay.
    OutputTokenReplay,
    /// Allow prompting and installing missing MCP dependencies.
    SkillMcpDependencyInstall,
    /// Allow request_user_input in Default collaboration mode.
    DefaultModeRequestUserInput,
    /// Allow root agents to send async user messages without model catalog support.
    SendMessageToUserAsync,
    /// Enable persisted thread goals and automatic goal continuation.
    Goals,
    /// Add current context-window metadata to model-visible context.
    TokenBudget,
    /// Enables experimental context management.
    ContextManagement,
    /// Track and report a shared token budget across a session's agent threads.
    RolloutBudget,
    /// Append trusted response configuration items when the selected reasoning effort changes.
    ReasoningEffortOverride,
    /// Add current-time reminders to model-visible context.
    CurrentTimeReminder,
    /// Report failed clock reads to the model without failing the turn.
    NonfatalClockReadErrors,
    /// Enable Fast mode selection in the TUI and request layer.
    FastMode,
    /// Enable Ultra Fast mode independently of Fast mode.
    UltrafastMode,
    /// Enable explicitly requested model changes for later step captures.
    StepModelSwitching,
    /// Removed compatibility key, still advertised to the Responses API.
    RemoteCompactionV2,
    /// Include retained images in the remote compaction context budget.
    CompactionImageBudget,
    /// Retain client-authored developer messages across compacted context windows.
    RetainClientDeveloperMessages,
    /// Honor client-requested tool-output retention across context windows.
    RetainClientToolOutputs,
    // Removed
}

impl Feature {
    pub fn key(self) -> &'static str {
        self.info().key
    }

    pub fn stage(self) -> Stage {
        self.info().stage
    }

    pub fn default_enabled(self) -> bool {
        self.info().default_enabled
    }

    fn info(self) -> &'static FeatureSpec {
        FEATURES
            .iter()
            .find(|spec| spec.id == self)
            .unwrap_or_else(|| unreachable!("missing FeatureSpec for {self:?}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegacyFeatureUsage {
    pub alias: String,
    pub feature: Feature,
    pub summary: String,
    pub details: Option<String>,
}

/// Holds the effective set of enabled features.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    enabled: BTreeSet<Feature>,
    legacy_usages: BTreeSet<LegacyFeatureUsage>,
}

#[derive(Debug, Clone, Default)]
pub struct FeatureOverrides {
    pub web_search_request: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureConfigSource<'a> {
    pub features: Option<&'a FeaturesToml>,
}

impl FeatureOverrides {
    fn apply(self, features: &mut Features) {
        if let Some(enabled) = self.web_search_request {
            if enabled {
                features.enable(Feature::WebSearchRequest);
            } else {
                features.disable(Feature::WebSearchRequest);
            }
            features.record_legacy_usage("web_search_request", Feature::WebSearchRequest);
        }
    }
}

impl Features {
    /// Starts with built-in defaults.
    pub fn with_defaults() -> Self {
        let mut set = BTreeSet::new();
        for spec in FEATURES {
            if spec.default_enabled {
                set.insert(spec.id);
            }
        }
        Self {
            enabled: set,
            legacy_usages: BTreeSet::new(),
        }
    }

    pub fn enabled(&self, f: Feature) -> bool {
        self.enabled.contains(&f)
    }

    /// Whether a routing tier is enabled, independently of model catalog support.
    pub fn service_tier_enabled(&self, service_tier: &str) -> bool {
        match service_tier {
            "flex" => true,
            "ultrafast" => self.enabled(Feature::UltrafastMode),
            _ => self.enabled(Feature::FastMode),
        }
    }

    /// Returns whether persistent execution is enabled for the selected effort.
    pub fn persistent_execution_enabled(&self, reasoning_effort: Option<&ReasoningEffort>) -> bool {
        reasoning_effort == Some(&ReasoningEffort::Persistent)
    }

    pub fn enable(&mut self, f: Feature) -> &mut Self {
        self.enabled.insert(f);
        self
    }

    pub fn disable(&mut self, f: Feature) -> &mut Self {
        self.enabled.remove(&f);
        self
    }

    pub fn set_enabled(&mut self, f: Feature, enabled: bool) -> &mut Self {
        if enabled {
            self.enable(f)
        } else {
            self.disable(f)
        }
    }

    pub fn record_legacy_usage_force(&mut self, alias: &str, feature: Feature) {
        let (summary, details) = legacy_usage_notice(alias, feature);
        self.legacy_usages.insert(LegacyFeatureUsage {
            alias: alias.to_string(),
            feature,
            summary,
            details,
        });
    }

    pub fn record_legacy_usage(&mut self, alias: &str, feature: Feature) {
        if alias == feature.key() {
            return;
        }
        self.record_legacy_usage_force(alias, feature);
    }

    pub fn legacy_feature_usages(&self) -> impl Iterator<Item = &LegacyFeatureUsage> + '_ {
        self.legacy_usages.iter()
    }

    /// Apply a table of key -> bool toggles (e.g. from TOML).
    pub fn apply_map(&mut self, m: &BTreeMap<String, bool>) {
        for (k, v) in m {
            match k.as_str() {
                "web_search_request" => {
                    self.record_legacy_usage_force(
                        "features.web_search_request",
                        Feature::WebSearchRequest,
                    );
                }
                "web_search_cached" => {
                    self.record_legacy_usage_force(
                        "features.web_search_cached",
                        Feature::WebSearchCached,
                    );
                }
                "unified_exec"
                | "shell_zsh_fork"
                | "unified_exec_zsh_fork"
                | "transcript_v2"
                | "tui_app_server" => {
                    continue;
                }
                "undo" => {
                    continue;
                }
                "js_repl" => {
                    continue;
                }
                "js_repl_tools_only" => {
                    continue;
                }
                "remote_control" | "remote_compaction_v2" => {
                    continue;
                }
                "apply_patch_freeform" => {
                    continue;
                }
                "tool_search" | "tool_search_always_defer_mcp_tools" => {
                    continue;
                }
                "image_detail_original" | "resize_all_images" | "item_ids" => {
                    continue;
                }
                "personality" | "plugin_hooks" => {
                    continue;
                }
                "skill_env_var_dependency_prompt" => {
                    continue;
                }
                "terminal_resize_reflow" => {
                    continue;
                }
                _ => {}
            }
            if k == "imagegenext" && m.contains_key(Feature::ImageGeneration.key()) {
                self.record_legacy_usage(k, Feature::ImageGeneration);
                continue;
            }
            match feature_for_key(k) {
                Some(feat) => {
                    if k != feat.key() {
                        self.record_legacy_usage(k.as_str(), feat);
                    }
                    if *v {
                        self.enable(feat);
                    } else {
                        self.disable(feat);
                    }
                }
                None => {
                    tracing::warn!("unknown feature key in config: {k}");
                }
            }
        }
    }

    pub fn from_sources(
        base: FeatureConfigSource<'_>,
        profile: FeatureConfigSource<'_>,
        overrides: FeatureOverrides,
    ) -> Self {
        let mut features = Features::with_defaults();

        for source in [base, profile] {
            if let Some(feature_entries) = source.features {
                features.apply_toml(feature_entries);
            }
        }

        overrides.apply(&mut features);

        features
    }

    pub fn enabled_features(&self) -> Vec<Feature> {
        self.enabled.iter().copied().collect()
    }
}

fn legacy_usage_notice(alias: &str, feature: Feature) -> (String, Option<String>) {
    let canonical = feature.key();
    match feature {
        Feature::WebSearchRequest | Feature::WebSearchCached => {
            let label = match alias {
                "web_search" => "[features].web_search",
                "features.web_search_request" | "web_search_request" => {
                    "[features].web_search_request"
                }
                "features.web_search_cached" | "web_search_cached" => {
                    "[features].web_search_cached"
                }
                _ => alias,
            };
            let summary =
                format!("`{label}` is deprecated because web search is enabled by default.");
            (summary, Some(web_search_details().to_string()))
        }
        _ => {
            let label = if alias.contains('.') || alias.starts_with('[') {
                alias.to_string()
            } else {
                format!("[features].{alias}")
            };
            let summary = format!("`{label}` is deprecated. Use `[features].{canonical}` instead.");
            let details = if alias == canonical {
                None
            } else {
                Some(format!(
                    "Enable it with `--enable {canonical}` or `[features].{canonical}` in config.toml. See https://developers.openai.com/codex/config-basic#feature-flags for details."
                ))
            };
            (summary, details)
        }
    }
}

fn web_search_details() -> &'static str {
    "Set `web_search` to `\"live\"`, `\"indexed\"`, `\"cached\"`, or `\"disabled\"` at the top level (or under a profile) in config.toml if you want to override it."
}

/// Keys accepted in `[features]` tables.
pub fn feature_for_key(key: &str) -> Option<Feature> {
    for spec in FEATURES {
        if spec.key == key {
            return Some(spec.id);
        }
    }
    legacy::feature_for_key(key)
}

pub fn canonical_feature_for_key(key: &str) -> Option<Feature> {
    FEATURES
        .iter()
        .find(|spec| spec.key == key)
        .map(|spec| spec.id)
}

/// Returns `true` if the provided string matches a known `[features]` key.
pub fn is_known_feature_key(key: &str) -> bool {
    key == "tool_registry" || feature_for_key(key).is_some()
}

/// Deserializable features table for TOML.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
pub struct FeaturesToml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_registry: Option<ToolRegistryConfigToml>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_prefixed_mcp_tool_names: Option<FeatureToml<NonPrefixedMcpToolNamesConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_v2: Option<FeatureToml<MultiAgentV2ConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<FeatureToml<TokenBudgetConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<FeatureToml<ContextManagementConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_budget: Option<FeatureToml<RolloutBudgetConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_time_reminder: Option<FeatureToml<CurrentTimeReminderConfigToml>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_tool: Option<FeatureToml<SleepToolConfigToml>>,
    /// Boolean feature toggles keyed by canonical or legacy feature name.
    #[serde(flatten)]
    entries: BTreeMap<String, bool>,
}

impl Features {
    fn apply_toml(&mut self, features: &FeaturesToml) {
        let entries = features.entries();
        self.apply_map(&entries);
    }
}

impl FeaturesToml {
    pub fn entries(&self) -> BTreeMap<String, bool> {
        let mut entries = self.entries.clone();
        if let Some(enabled) = self
            .non_prefixed_mcp_tool_names
            .as_ref()
            .and_then(FeatureToml::enabled)
        {
            entries.insert(Feature::NonPrefixedMcpToolNames.key().to_string(), enabled);
        }
        if let Some(enabled) = self.multi_agent_v2.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::MultiAgentV2.key().to_string(), enabled);
        }
        if let Some(enabled) = self.token_budget.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::TokenBudget.key().to_string(), enabled);
        }
        if let Some(enabled) = self
            .context_management
            .as_ref()
            .and_then(FeatureToml::enabled)
        {
            entries.insert(Feature::ContextManagement.key().to_string(), enabled);
        }
        if let Some(enabled) = self.rollout_budget.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::RolloutBudget.key().to_string(), enabled);
        }
        if let Some(enabled) = self
            .current_time_reminder
            .as_ref()
            .and_then(FeatureToml::enabled)
        {
            entries.insert(Feature::CurrentTimeReminder.key().to_string(), enabled);
        }
        if let Some(enabled) = self.sleep_tool.as_ref().and_then(FeatureToml::enabled) {
            entries.insert(Feature::SleepTool.key().to_string(), enabled);
        }
        entries
    }
}

impl From<BTreeMap<String, bool>> for FeaturesToml {
    fn from(entries: BTreeMap<String, bool>) -> Self {
        Self {
            entries,
            ..Default::default()
        }
    }
}

// To be used for features that need more configuration than just enabled/disabled and
// require a custom config struct under `[features]`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum FeatureToml<T> {
    Enabled(bool),
    Config(T),
}

impl<T: FeatureConfig> FeatureToml<T> {
    pub fn enabled(&self) -> Option<bool> {
        match self {
            Self::Enabled(enabled) => Some(*enabled),
            Self::Config(config) => config.enabled(),
        }
    }
}

// A trait to be implemented by custom feature config structs when defining a feature that needs more configuration than
// just enabled/disabled.
pub trait FeatureConfig {
    fn enabled(&self) -> Option<bool>;
}

/// Single, easy-to-read registry of all feature definitions.
#[derive(Debug, Clone, Copy)]
pub struct FeatureSpec {
    pub id: Feature,
    pub key: &'static str,
    pub stage: Stage,
    pub default_enabled: bool,
}

pub const FEATURES: &[FeatureSpec] = &[
    // Stable features.
    FeatureSpec {
        id: Feature::ShellTool,
        key: "shell_tool",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ViewImage,
        key: "view_image",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::SleepTool,
        key: "sleep_tool",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::SecretAuthStorage,
        key: "secret_auth_storage",
        stage: Stage::Stable,
        default_enabled: cfg!(windows),
    },
    FeatureSpec {
        id: Feature::UnifiedExecTty,
        key: "unified_exec_tty",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ShellSnapshot,
        key: "shell_snapshot",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::LoginShellPackagePath,
        key: "login_shell_package_path",
        stage: Stage::Experimental {
            name: "Bundled tools in login shells",
            menu_description: "Keep bundled tools such as ripgrep available when login shell startup resets PATH.",
            announcement: "",
        },
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::PowerShellShellVersion,
        key: "powershell_shell_version",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ShellSnapshotV2,
        key: "shell_snapshot_v2",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CwdRelativeTurnDiffs,
        key: "cwd_relative_turn_diffs",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ContentItemKinds,
        key: "content_item_kinds",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ExecutedToolCallMetadata,
        key: "executed_tool_call_metadata",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::InstantInterrupt,
        key: "instant_interrupt",
        stage: Stage::UnderDevelopment,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::WebSearchRequest,
        key: "web_search_request",
        stage: Stage::Deprecated,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::WebSearchCached,
        key: "web_search_cached",
        stage: Stage::Deprecated,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::StandaloneWebSearch,
        key: "standalone_web_search",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RuntimeMetrics,
        key: "runtime_metrics",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ApplyPatchStreamingEvents,
        key: "apply_patch_streaming_events",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CodexHooks,
        key: "hooks",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ApiKeyModelDiscovery,
        key: "api_key_model_discovery",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ApiKeyCyberAccessPrograms,
        key: "api_key_cyber_access_programs",
        stage: Stage::Stable,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::EnableRequestCompression,
        key: "enable_request_compression",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::UnboundedConnectionRetries,
        key: "unbounded_connection_retries",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::RespectSystemProxy,
        key: "respect_system_proxy",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SystemProxyFallback,
        key: "system_proxy_fallback",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::Collab,
        key: "multi_agent",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::MultiAgentV2,
        key: "multi_agent_v2",
        stage: Stage::Stable,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ModelCatalogInContext,
        key: "model_catalog_in_context",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::MultiAgentV2DynamicTools,
        key: "multi_agent_v2_dynamic_tools",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SubagentDefaultContextLimits,
        key: "subagent_default_context_limits",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::DeferMailboxPreemption,
        key: "defer_mailbox_preemption",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::AgentMessageBoard,
        key: "agent_message_board",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Psp,
        key: "psp",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Mcp20260728,
        key: "mcp_2026_07_28",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::McpOAuthRefreshCoordination,
        key: "mcp_oauth_refresh_coordination",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::UseXaa,
        key: "use_xaa",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::DeferredToolWorldState,
        key: "deferred_tool_world_state",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::IncrementalTools,
        key: "incremental_tools",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::NonPrefixedMcpToolNames,
        key: "non_prefixed_mcp_tool_names",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SkipHostSkillDiscovery,
        key: "skip_host_skill_discovery",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ImageGeneration,
        key: "image_generation",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::ImageResizeNotice,
        key: "image_resize_notice",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::UnifiedImageBudget,
        key: "unified_image_budget",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ConcurrentReasoningSummaries,
        key: "concurrent_reasoning_summaries",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::OutputTokenReplay,
        key: "output_token_replay",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SkillMcpDependencyInstall,
        key: "skill_mcp_dependency_install",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::DefaultModeRequestUserInput,
        key: "default_mode_request_user_input",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::SendMessageToUserAsync,
        key: "send_message_to_user_async",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::Goals,
        key: "goals",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::TokenBudget,
        key: "token_budget",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ContextManagement,
        key: "context_management",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RolloutBudget,
        key: "rollout_budget",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::ReasoningEffortOverride,
        key: "reasoning_effort_override",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CurrentTimeReminder,
        key: "current_time_reminder",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::NonfatalClockReadErrors,
        key: "nonfatal_clock_read_errors",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::FastMode,
        key: "fast_mode",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::UltrafastMode,
        key: "ultrafast_mode",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::StepModelSwitching,
        key: "step_model_switching",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RemoteCompactionV2,
        key: "remote_compaction_v2",
        stage: Stage::Removed,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::CompactionImageBudget,
        key: "compaction_image_budget",
        stage: Stage::Stable,
        default_enabled: true,
    },
    FeatureSpec {
        id: Feature::RetainClientDeveloperMessages,
        key: "retain_client_developer_messages",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
    FeatureSpec {
        id: Feature::RetainClientToolOutputs,
        key: "retain_client_tool_outputs",
        stage: Stage::UnderDevelopment,
        default_enabled: false,
    },
];

pub fn unstable_features_warning_event(
    effective_features: Option<&Table>,
    suppress_unstable_features_warning: bool,
    features: &Features,
    config_path: &str,
) -> Option<Event> {
    if suppress_unstable_features_warning {
        return None;
    }

    let mut under_development_feature_keys = Vec::new();
    if let Some(table) = effective_features {
        for (key, value) in table {
            let is_enabled = value.as_bool() == Some(true)
                || value
                    .as_table()
                    .and_then(|table| table.get("enabled"))
                    .and_then(toml::Value::as_bool)
                    == Some(true);
            if !is_enabled {
                continue;
            }
            let Some(spec) = FEATURES.iter().find(|spec| spec.key == key.as_str()) else {
                continue;
            };
            if !features.enabled(spec.id) {
                continue;
            }
            if matches!(spec.stage, Stage::UnderDevelopment) {
                under_development_feature_keys.push(spec.key.to_string());
            }
        }
    }

    if under_development_feature_keys.is_empty() {
        return None;
    }

    under_development_feature_keys.sort();
    let under_development_feature_keys = under_development_feature_keys.join(", ");
    let message = format!(
        "Under-development features enabled: {under_development_feature_keys}. Under-development features are incomplete and may behave unpredictably. To suppress this warning, set `suppress_unstable_features_warning = true` in {config_path}."
    );
    Some(Event {
        id: String::new(),
        msg: EventMsg::Warning(WarningEvent { message }),
    })
}

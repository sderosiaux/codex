//! Schema-heavy configuration TOML types used by Codex.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU64;

use crate::HooksToml;
use crate::profile_toml::ConfigProfile;
use crate::types::AnalyticsConfigToml;
use crate::types::AppsConfigToml;
use crate::types::AuthCredentialsStoreMode;
use crate::types::History;
use crate::types::MarketplaceConfig;
use crate::types::McpEnterpriseManagedAuthConfig;
use crate::types::McpServerConfig;
use crate::types::MemoriesToml;
use crate::types::OAuthCredentialsStoreMode;
use crate::types::OtelConfigToml;
use crate::types::PluginsConfigToml;
use crate::types::ShellEnvironmentPolicyToml;
use crate::types::SkillsConfig;
use crate::types::ToolSuggestConfig;
use codex_features::FeaturesToml;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::AMAZON_BEDROCK_RUNTIME_PROVIDER_ID;
use codex_model_provider_info::LEGACY_OLLAMA_CHAT_PROVIDER_ID;
use codex_model_provider_info::LMSTUDIO_OSS_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_CHAT_PROVIDER_REMOVED_ERROR;
use codex_model_provider_info::OLLAMA_OSS_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::config_types::WebSearchToolConfig;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as SerdeError;

const RESERVED_MODEL_PROVIDER_IDS: [&str; 5] = [
    AMAZON_BEDROCK_PROVIDER_ID,
    AMAZON_BEDROCK_RUNTIME_PROVIDER_ID,
    OPENAI_PROVIDER_ID,
    OLLAMA_OSS_PROVIDER_ID,
    LMSTUDIO_OSS_PROVIDER_ID,
];

pub const DEFAULT_PROJECT_DOC_MAX_BYTES: usize = 32 * 1024;

fn default_history() -> Option<History> {
    Some(History::default())
}

const fn default_project_doc_max_bytes() -> Option<usize> {
    Some(DEFAULT_PROJECT_DOC_MAX_BYTES)
}

fn default_project_doc_fallback_filenames() -> Option<Vec<String>> {
    Some(Vec::new())
}

const fn default_hide_agent_reasoning() -> Option<bool> {
    Some(false)
}

const fn default_true() -> bool {
    true
}

/// Backward-compatible shape for ChatGPT workspace login restrictions in config.toml.
#[derive(Serialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(untagged)]
pub enum ForcedChatgptWorkspaceIds {
    Single(String),
    Multiple(Vec<String>),
}

impl ForcedChatgptWorkspaceIds {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(value) => vec![value],
            Self::Multiple(values) => values,
        }
    }
}

impl<'de> Deserialize<'de> for ForcedChatgptWorkspaceIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Single(String),
            Multiple(Vec<String>),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Single(value) if value.contains(',') => Err(D::Error::custom(
                "forced_chatgpt_workspace_id must be a single workspace ID string or a TOML list \
of strings; comma-separated strings are not supported. Use \
`forced_chatgpt_workspace_id = [\"123e4567-e89b-42d3-a456-426614174000\", \
\"123e4567-e89b-42d3-a456-426614174001\"]` instead.",
            )),
            Repr::Single(value) => Ok(Self::Single(value)),
            Repr::Multiple(values) => Ok(Self::Multiple(values)),
        }
    }
}

/// Orchestrator-owned feature settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorToml {
    /// Legacy no-op setting retained for compatibility. Use `cloud.skills` to configure cloud skills.
    pub skills: Option<FeatureToggleToml>,
    pub mcp: Option<FeatureToggleToml>,
}

/// Cloud-owned feature settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct CloudToml {
    /// Cloud skills are permitted by default; the host must supply a cloud provider.
    pub skills: Option<FeatureToggleToml>,
}

/// Optional enablement of a configured feature.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct FeatureToggleToml {
    pub enabled: Option<bool>,
}

/// Base config deserialized from ~/.codex/config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigToml {
    /// Optional override of model selection.
    pub model: Option<String>,
    /// Review model override used by the `/review` feature.
    pub review_model: Option<String>,

    /// Provider to use from the model_providers map.
    pub model_provider: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether the auto-compaction limit applies to the full context or
    /// only to tokens after the carried prefix in the current compaction window.
    pub model_auto_compact_token_limit_scope: Option<AutoCompactTokenLimitScope>,

    /// Percentage of the usable context window that triggers compaction after a final
    /// response. Existing auto-compaction limits still apply. Omitted or zero disables
    /// turn-end compaction; valid values are 0–100.
    #[schemars(range(min = 0, max = 100))]
    pub model_post_turn_compact_threshold_percent: Option<u8>,

    #[serde(default)]
    pub shell_environment_policy: ShellEnvironmentPolicyToml,

    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    pub allow_login_shell: Option<bool>,

    /// Optional external command to spawn for end-user notifications.
    #[serde(default)]
    pub notify: Option<Vec<String>>,

    /// System instructions.
    pub instructions: Option<String>,

    /// Developer instructions inserted as a `developer` role message.
    #[serde(default)]
    pub developer_instructions: Option<String>,

    /// Whether to inject the `<apps_instructions>` developer block.
    pub include_apps_instructions: Option<bool>,

    /// Whether to inject the `<collaboration_mode>` developer block.
    pub include_collaboration_mode_instructions: Option<bool>,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: Option<bool>,

    /// Whether environment context includes the current date and timezone.
    pub include_environment_context_time: Option<bool>,

    /// Optional path to a file containing model instructions that will override
    /// the built-in instructions for the selected model. Users are STRONGLY
    /// DISCOURAGED from using this field, as deviating from the instructions
    /// sanctioned by Codex will likely degrade model performance.
    pub model_instructions_file: Option<AbsolutePathBuf>,

    /// Compact prompt used for history compaction.
    pub compact_prompt: Option<String>,

    /// When set, restricts ChatGPT login to one or more workspace identifiers.
    #[serde(default)]
    pub forced_chatgpt_workspace_id: Option<ForcedChatgptWorkspaceIds>,

    /// When set, restricts the login mechanism users may use.
    #[serde(default)]
    pub forced_login_method: Option<ForcedLoginMethod>,

    /// Preferred backend for storing CLI auth credentials.
    /// file (default): Use a file in the Codex home directory.
    /// keyring: Use an OS-specific keyring service.
    /// auto: Use the keyring if available, otherwise use a file.
    #[serde(default)]
    pub cli_auth_credentials_store: Option<AuthCredentialsStoreMode>,

    /// Definition for MCP servers that Codex can reach out to for tool calls.
    #[serde(default)]
    // Uses the raw MCP input shape (custom deserialization) rather than `McpServerConfig`.
    #[schemars(schema_with = "crate::schema::mcp_servers_schema")]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Trusted enterprise IdP shared by EMA-enabled MCP servers and plugins.
    #[serde(default)]
    pub mcp_enterprise_managed_auth: Option<McpEnterpriseManagedAuthConfig>,

    /// Preferred backend for storing MCP OAuth credentials.
    /// keyring: Use an OS-specific keyring service.
    ///          https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/oauth.rs#L2
    /// file: Use a file in the Codex home directory.
    /// auto (default): Use the OS-specific keyring service if available, otherwise use a file.
    #[serde(default)]
    pub mcp_oauth_credentials_store: Option<OAuthCredentialsStoreMode>,

    /// Optional fixed port for the local HTTP callback server used during MCP OAuth login.
    /// When unset, Codex will bind to an ephemeral port chosen by the OS.
    pub mcp_oauth_callback_port: Option<u16>,

    /// Optional redirect URI to use during MCP OAuth login.
    /// When set, this URI is used in the OAuth authorization request instead
    /// of the local listener address. The local callback listener still binds
    /// to 127.0.0.1 (using `mcp_oauth_callback_port` when provided).
    pub mcp_oauth_callback_url: Option<String>,

    /// Milliseconds to wait for optional MCP servers while building the initial tool catalog.
    ///
    /// Defaults to 1000. Set to 0 to disable the shared grace and wait for each
    /// server's configured `startup_timeout_sec` instead.
    pub mcp_optional_startup_grace_ms: Option<u64>,

    /// User-defined provider entries that extend the built-in list. Built-in
    /// IDs cannot be overridden.
    #[serde(default, deserialize_with = "deserialize_model_providers")]
    pub model_providers: HashMap<String, ModelProviderInfo>,

    /// Maximum total bytes of project instruction content across all selected environments.
    #[serde(default = "default_project_doc_max_bytes")]
    pub project_doc_max_bytes: Option<usize>,

    /// Ordered list of fallback filenames to look for when AGENTS.md is missing.
    #[serde(default = "default_project_doc_fallback_filenames")]
    pub project_doc_fallback_filenames: Option<Vec<String>>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: Option<u64>,

    /// Seconds a thread must have no subscribers and no activity before app-server
    /// unloads it. Defaults to 1800 (30 minutes); zero unloads immediately.
    /// Changes require a server restart.
    pub thread_unload_delay_secs: Option<u64>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_path: Option<AbsolutePathBuf>,

    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_module_dirs: Option<Vec<AbsolutePathBuf>>,

    /// Profile to use from the `profiles` map.
    pub profile: Option<String>,

    /// Named profiles to facilitate switching between different configurations.
    #[serde(default)]
    pub profiles: HashMap<String, ConfigProfile>,

    /// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
    #[serde(default = "default_history")]
    pub history: Option<History>,

    /// Directory where Codex stores the SQLite state DB.
    /// Defaults to `$CODEX_SQLITE_HOME` when set. Otherwise uses `$CODEX_HOME`.
    pub sqlite_home: Option<AbsolutePathBuf>,

    /// Directory where Codex writes log files. Setting this value explicitly
    /// also enables the TUI text log in this directory.
    /// Defaults to `$CODEX_HOME/log`.
    pub log_dir: Option<AbsolutePathBuf>,

    /// When set to `true`, `AgentReasoning` events will be hidden from the
    /// UI/output. Defaults to `false`.
    #[serde(default = "default_hide_agent_reasoning")]
    pub hide_agent_reasoning: Option<bool>,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: Option<bool>,

    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Optional path to a JSON model catalog (applied on startup only).
    /// Per-thread `config` overrides are accepted but do not reapply this (no-ops).
    pub model_catalog_json: Option<AbsolutePathBuf>,

    /// Deprecated: `friendly` and `pragmatic` no longer select a style.
    pub personality: Option<Personality>,

    /// Optional explicit service tier request id for new turns (for example
    /// `default`, `priority`, or `flex`; legacy `fast` also works).
    pub service_tier: Option<String>,

    /// Base URL for requests to ChatGPT (as opposed to the OpenAI API).
    pub chatgpt_base_url: Option<String>,

    /// Optional product SKU forwarded on host-owned Codex Apps MCP requests.
    pub apps_mcp_product_sku: Option<String>,

    /// Bounded, product-owned metadata attached to every Responses API request.
    pub responses_api_metadata: Option<BTreeMap<String, String>>,

    /// Orchestrator-owned feature settings.
    pub orchestrator: Option<OrchestratorToml>,

    /// Cloud-owned feature settings.
    pub cloud: Option<CloudToml>,

    /// Base URL override for the built-in `openai` model provider.
    pub openai_base_url: Option<String>,

    /// Removed. Former remote thread-store endpoint setting kept only so we can
    /// fail fast instead of silently falling back to local persistence.
    #[schemars(skip)]
    pub experimental_thread_store_endpoint: Option<String>,

    /// Experimental / do not use. Selects the thread store implementation.
    pub experimental_thread_store: Option<ThreadStoreToml>,

    /// Controls the web search tool mode: disabled, cached, indexed, or live.
    pub web_search: Option<WebSearchMode>,

    /// Nested tools section for feature toggles
    pub tools: Option<ToolsToml>,

    /// Additional discoverable tools that can be suggested for installation.
    pub tool_suggest: Option<ToolSuggestConfig>,

    /// Agent-related settings (thread limits, etc.).
    pub agents: Option<AgentsToml>,

    /// Goal-related settings.
    pub goals: Option<GoalsToml>,

    /// Memories subsystem settings.
    pub memories: Option<MemoriesToml>,

    /// User-level skill config entries keyed by SKILL.md path.
    pub skills: Option<SkillsConfig>,

    /// Lifecycle hooks configured inline in TOML plus user-level overrides.
    pub hooks: Option<HooksToml>,

    /// Default and per-plugin settings keyed by exact `<plugin>@<marketplace>` IDs.
    #[serde(default)]
    pub plugins: PluginsConfigToml,

    /// User-level marketplace entries keyed by marketplace name.
    #[serde(default)]
    pub marketplaces: HashMap<String, MarketplaceConfig>,

    /// Centralized feature flags (new). Prefer this over individual toggles.
    #[serde(default)]
    // Injects known feature keys into the schema and forbids unknown keys.
    #[schemars(schema_with = "crate::schema::features_schema")]
    pub features: Option<FeaturesToml>,

    /// Suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: Option<bool>,

    /// Compatibility-only settings retained so legacy `ghost_snapshot`
    /// config still loads.
    #[serde(default)]
    pub ghost_snapshot: Option<GhostSnapshotToml>,

    /// Markers used to detect the project root when searching parent
    /// directories for `.codex` folders. Defaults to [".git"] when unset.
    #[serde(default)]
    pub project_root_markers: Option<Vec<String>>,

    /// When `enabled` is `false`, disables OpenAI analytics across Codex product surfaces on this machine.
    /// Custom OTLP metrics exporters are controlled by `otel.metrics_exporter`.
    /// When unset, the default depends on the client.
    pub analytics: Option<AnalyticsConfigToml>,

    /// Settings for app-specific controls.
    #[serde(default)]
    pub apps: Option<AppsConfigToml>,

    /// OTEL configuration.
    pub otel: Option<OtelConfigToml>,

    pub experimental_compact_prompt_file: Option<AbsolutePathBuf>,
    /// Preferred OSS provider for local models, e.g. "lmstudio" or "ollama".
    pub oss_provider: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadStoreToml {
    Local {},
    #[schemars(skip)]
    InMemory {
        id: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolsToml {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_web_search_tool_config"
    )]
    pub web_search: Option<WebSearchToolConfig>,
    pub experimental_request_user_input: Option<ExperimentalRequestUserInput>,
    pub update_plan: Option<UpdatePlanToolConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExperimentalRequestUserInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct UpdatePlanToolConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WebSearchToolConfigInput {
    Enabled(bool),
    Config(WebSearchToolConfig),
}

fn deserialize_optional_web_search_tool_config<'de, D>(
    deserializer: D,
) -> Result<Option<WebSearchToolConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<WebSearchToolConfigInput>::deserialize(deserializer)?;

    Ok(match value {
        None => None,
        Some(WebSearchToolConfigInput::Enabled(enabled)) => {
            let _ = enabled;
            None
        }
        Some(WebSearchToolConfigInput::Config(config)) => Some(config),
    })
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GoalsToml {
    /// Maximum token budget allowed for a goal and default budget for new goals.
    pub max_goal_token_budget: Option<NonZeroU64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentsToml {
    /// Whether multi-agent tools are enabled. Defaults to true.
    /// An enabled `features.multi_agent_v2` setting takes precedence.
    pub enabled: Option<bool>,
    /// Maximum number of spawned agent threads that can be open concurrently per session.
    /// When unset, the selected multi-agent backend uses its default.
    #[serde(alias = "max_threads")]
    #[schemars(range(min = 1))]
    pub max_concurrent_threads_per_session: Option<usize>,
    /// Maximum nesting depth for V1 agent threads. Ignored by V2.
    pub max_depth: Option<i32>,
    /// Default model for spawned subagents when the spawn call does not select one.
    pub default_subagent_model: Option<String>,
    /// Default reasoning effort for spawned subagents when the spawn call does not select one.
    pub default_subagent_reasoning_effort: Option<ReasoningEffort>,
    /// Removed agent-job setting retained as a no-op for compatibility.
    #[schemars(skip)]
    pub job_max_runtime_seconds: Option<u64>,
    /// Whether to record a model-visible message when an agent turn is interrupted.
    /// Defaults to true.
    pub interrupt_message: Option<bool>,

    /// User-defined role declarations keyed by role name.
    ///
    /// Example:
    /// ```toml
    /// [agents.researcher]
    /// description = "Research-focused role."
    /// config_file = "./agents/researcher.toml"
    /// nickname_candidates = ["Herodotus", "Ibn Battuta"]
    /// ```
    #[serde(default, flatten)]
    pub roles: BTreeMap<String, AgentRoleToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AgentRoleToml {
    /// Human-facing role documentation used in spawn tool guidance.
    /// Required unless supplied by the referenced agent role file.
    pub description: Option<String>,

    /// Path to a role-specific config layer.
    /// Relative paths are resolved relative to the `config.toml` that defines them.
    pub config_file: Option<AbsolutePathBuf>,

    /// Candidate nicknames for agents spawned with this role.
    pub nickname_candidates: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct GhostSnapshotToml {
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "ignore_untracked_files_over_bytes")]
    pub ignore_large_untracked_files: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    #[serde(alias = "large_untracked_dir_warning_threshold")]
    pub ignore_large_untracked_dirs: Option<i64>,
    /// Legacy no-op setting retained for compatibility.
    pub disable_warnings: Option<bool>,
}

pub fn validate_reserved_model_provider_ids(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    let mut conflicts = model_providers
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
            ) && RESERVED_MODEL_PROVIDER_IDS.contains(&key.as_str())
        })
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>();
    conflicts.sort_unstable();
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "model_providers contains reserved built-in provider IDs: {}. \
Built-in providers cannot be overridden. Rename your custom provider (for example, `openai-custom`).",
            conflicts.join(", ")
        ))
    }
}

pub fn validate_model_providers(
    model_providers: &HashMap<String, ModelProviderInfo>,
) -> Result<(), String> {
    validate_reserved_model_provider_ids(model_providers)?;
    for (key, provider) in model_providers {
        if matches!(
            key.as_str(),
            AMAZON_BEDROCK_PROVIDER_ID | AMAZON_BEDROCK_RUNTIME_PROVIDER_ID
        ) {
            provider
                .validate_bedrock_override()
                .map_err(|message| format!("model_providers.{key} {message}"))?;
        } else {
            if provider.aws.is_some() {
                return Err(format!(
                    "model_providers.{key}: provider aws is only supported for \
`{AMAZON_BEDROCK_PROVIDER_ID}` or `{AMAZON_BEDROCK_RUNTIME_PROVIDER_ID}`"
                ));
            }
            if provider.name.trim().is_empty() {
                return Err(format!(
                    "model_providers.{key}: provider name must not be empty"
                ));
            }
        }
        provider
            .validate()
            .map_err(|message| format!("model_providers.{key}: {message}"))?;
    }
    Ok(())
}

fn deserialize_model_providers<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, ModelProviderInfo>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let model_providers = HashMap::<String, ModelProviderInfo>::deserialize(deserializer)?;
    validate_model_providers(&model_providers).map_err(serde::de::Error::custom)?;
    Ok(model_providers)
}

#[cfg(test)]
#[path = "bedrock_runtime_tests.rs"]
mod bedrock_runtime_tests;

pub fn validate_oss_provider(provider: &str) -> std::io::Result<()> {
    match provider {
        LMSTUDIO_OSS_PROVIDER_ID | OLLAMA_OSS_PROVIDER_ID => Ok(()),
        LEGACY_OLLAMA_CHAT_PROVIDER_ID => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            OLLAMA_CHAT_PROVIDER_REMOVED_ERROR,
        )),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Invalid OSS provider '{provider}'. Must be one of: {LMSTUDIO_OSS_PROVIDER_ID}, {OLLAMA_OSS_PROVIDER_ID}"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const WORKSPACE_ID_A: &str = "123e4567-e89b-42d3-a456-426614174000";
    const WORKSPACE_ID_B: &str = "123e4567-e89b-42d3-a456-426614174001";

    #[test]
    fn thread_unload_delay_requires_nonnegative_seconds() {
        for value in ["-1", "1.5", "\"60\""] {
            let error =
                toml::from_str::<ConfigToml>(&format!("thread_unload_delay_secs = {value}"))
                    .expect_err("idle timeout must be a nonnegative integer");
            assert!(error.to_string().contains("thread_unload_delay_secs"));
        }
    }

    #[test]
    fn forced_chatgpt_workspace_id_accepts_single_string() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A}""#
        ))
        .expect("single workspace id should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace id should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_accepts_string_list() {
        let config: ConfigToml = toml::from_str(&format!(
            r#"forced_chatgpt_workspace_id = ["{WORKSPACE_ID_A}", "{WORKSPACE_ID_B}"]"#
        ))
        .expect("workspace id list should deserialize");

        assert_eq!(
            config
                .forced_chatgpt_workspace_id
                .expect("workspace ids should be set")
                .into_vec(),
            vec![WORKSPACE_ID_A.to_string(), WORKSPACE_ID_B.to_string()]
        );
    }

    #[test]
    fn forced_chatgpt_workspace_id_rejects_comma_separated_string() {
        let err = toml::from_str::<ConfigToml>(&format!(
            r#"forced_chatgpt_workspace_id = "{WORKSPACE_ID_A},{WORKSPACE_ID_B}""#
        ))
        .expect_err("comma-separated string should be rejected");

        let message = err.to_string();
        assert!(message.contains("TOML list of strings"));
        assert!(message.contains("comma-separated strings are not supported"));
    }

    #[test]
    fn amazon_bedrock_auth_command_must_not_be_empty() {
        let err = toml::from_str::<ConfigToml>(
            r#"
[model_providers.amazon-bedrock.auth]
command = "   "
"#,
        )
        .expect_err("empty Amazon Bedrock auth command should be rejected");

        assert!(
            err.to_string().contains(
                "model_providers.amazon-bedrock: provider auth.command must not be empty"
            )
        );
    }
}

use crate::config::edit::ConfigEdit;
use crate::config::edit::ConfigEditsBuilder;
use crate::path_utils::normalize_for_native_workdir;
use crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use codex_agent_roles::load_agent_roles;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::McpEnterpriseManagedAuthConfig;
use codex_config::ProfileV2Name;
use codex_config::ThreadConfigLoader;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::DEFAULT_PROJECT_DOC_MAX_BYTES;
use codex_config::config_toml::ThreadStoreToml;
use codex_config::config_toml::validate_model_providers;
use codex_config::loader::load_config_layers_state;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::History;
use codex_config::types::McpServerConfig;
use codex_config::types::MemoriesConfig;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LOCAL_FS;
use codex_exec_server::ReadFileOptions;
use codex_features::CurrentTimeReminderConfigToml;
use codex_features::CurrentTimeReminderDeliveryMode;
use codex_features::CurrentTimeSource;
use codex_features::Feature;
use codex_features::FeatureConfigSource;
use codex_features::FeatureOverrides;
use codex_features::FeatureToml;
use codex_features::Features;
use codex_features::FeaturesToml;
use codex_features::MultiAgentV2ConfigToml;
use codex_features::SleepToolMode;
use codex_features::TokenBudgetConfigToml;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthManagerConfig;
use codex_login::AuthRouteConfig;
use codex_mcp::DEFAULT_OPTIONAL_MCP_STARTUP_GRACE;
use codex_mcp::McpConfig;
use codex_mcp::McpProtocolMode;
use codex_mcp::McpServerRegistration;
use codex_mcp::ResolvedMcpCatalog;
use codex_model_provider::ProviderCapabilities;
use codex_model_provider_info::LEGACY_OLLAMA_CHAT_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OLLAMA_CHAT_PROVIDER_REMOVED_ERROR;
use codex_model_provider_info::built_in_model_providers;
use codex_model_provider_info::merge_configured_model_providers;
use codex_models_manager::ModelsManagerConfig;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchConfig;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::MultiAgentVersion;
use codex_rmcp_client::McpOAuthRefreshMode;
pub use codex_thread_store::ExtraConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use codex_utils_path_uri::PathUri;
use http::HeaderValue;
use rmcp::model::ElicitationCapability;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::responses_metadata::validate_extra_metadata;
use toml::Value as TomlValue;

mod auth_keyring;
pub mod edit;
mod metrics;
mod otel;
mod runtime_refresh;
pub(crate) use runtime_refresh::RuntimeConfigRefresh;
mod token_budget_startup;
pub use auth_keyring::bootstrap_auth_config;
pub use auth_keyring::resolve_bootstrap_auth_keyring_backend_kind;
pub use codex_agent_roles::AgentRoleConfig;
pub use codex_config::ConfigLoadOptions;
pub use codex_config::LoaderOverrides;
pub(crate) use metrics::emit_session_start_metrics;
pub use token_budget_startup::TokenBudgetStartupConfig;

const DEFAULT_IGNORE_LARGE_UNTRACKED_DIRS: i64 = 200;
const DEFAULT_IGNORE_LARGE_UNTRACKED_FILES: i64 = 10 * 1024 * 1024;

/// Compatibility-only config retained so legacy `ghost_snapshot` settings
/// continue to load even though snapshots are no longer produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostSnapshotConfig {
    pub ignore_large_untracked_files: Option<i64>,
    pub ignore_large_untracked_dirs: Option<i64>,
    pub disable_warnings: bool,
}

impl Default for GhostSnapshotConfig {
    fn default() -> Self {
        Self {
            ignore_large_untracked_files: Some(DEFAULT_IGNORE_LARGE_UNTRACKED_FILES),
            ignore_large_untracked_dirs: Some(DEFAULT_IGNORE_LARGE_UNTRACKED_DIRS),
            disable_warnings: false,
        }
    }
}

/// Maximum number of bytes of the documentation that will be embedded. Larger
/// files are *silently truncated* to this size so we do not take up too much of
/// the context window.
pub(crate) const AGENTS_MD_MAX_BYTES: usize = DEFAULT_PROJECT_DOC_MAX_BYTES; // 32 KiB
pub(crate) const DEFAULT_AGENT_MAX_THREADS: Option<usize> = Some(6);
pub(crate) const DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION: usize = 4;
pub(crate) const DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS: i64 = 10_000;
pub(crate) const DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS: i64 = 3600 * 1000;
pub(crate) const DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS: i64 = 30_000;
const DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE: &str = "collaboration";

pub(crate) const HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS: i64 = 0;
pub(crate) const HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS: i64 =
    DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS;
pub(crate) const DEFAULT_AGENT_MAX_DEPTH: i32 = 1;
const LOCAL_DEV_BUILD_VERSION: &str = "0.0.0";

pub const CONFIG_TOML_FILE: &str = "config.toml";
const CONFIG_PROFILE_V2_SUFFIX: &str = ".config.toml";

fn resolve_sqlite_home_env(resolved_cwd: &Path) -> Option<AbsolutePathBuf> {
    let raw = std::env::var(codex_state::SQLITE_HOME_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(AbsolutePathBuf::resolve_path_against_base(
        trimmed,
        resolved_cwd,
    ))
}

fn resolve_cli_auth_credentials_store_mode(
    configured: AuthCredentialsStoreMode,
    package_version: &str,
) -> AuthCredentialsStoreMode {
    match (package_version, configured) {
        (
            LOCAL_DEV_BUILD_VERSION,
            AuthCredentialsStoreMode::Keyring | AuthCredentialsStoreMode::Auto,
        ) => AuthCredentialsStoreMode::File,
        (_, mode) => mode,
    }
}

fn resolve_mcp_oauth_credentials_store_mode(
    configured: OAuthCredentialsStoreMode,
    package_version: &str,
) -> OAuthCredentialsStoreMode {
    match (package_version, configured) {
        (
            LOCAL_DEV_BUILD_VERSION,
            OAuthCredentialsStoreMode::Keyring | OAuthCredentialsStoreMode::Auto,
        ) => OAuthCredentialsStoreMode::File,
        (_, mode) => mode,
    }
}

/// Configured thread persistence backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThreadStoreConfig {
    /// Persist threads locally using rollout JSONL files and sqlite metadata.
    #[default]
    Local,
    /// In-memory thread store for test and debug configurations.
    InMemory { id: String },
}

/// Application configuration loaded from disk and merged with overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Auth bootstrap routing installed by the app-server configuration owner.
    pub application_auth_route_config: Option<AuthRouteConfig>,
    /// Provenance for how this [`Config`] was derived (merged layers).
    pub config_layer_stack: ConfigLayerStack,

    /// Warnings collected during config load that should be shown on startup.
    pub startup_warnings: Vec<String>,

    /// Optional override of model selection.
    pub model: Option<String>,

    /// Effective service tier request id preference for new turns.
    /// `default` means the user explicitly selected standard routing.
    pub service_tier: Option<String>,

    /// Model used specifically for review sessions.
    pub review_model: Option<String>,

    /// Size of the context window for the model, in tokens.
    pub model_context_window: Option<i64>,

    /// Token usage threshold triggering auto-compaction of conversation history.
    pub model_auto_compact_token_limit: Option<i64>,

    /// Controls whether `model_auto_compact_token_limit` applies to the full
    /// active context or only tokens after the carried compaction-window prefix.
    pub model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope,

    /// Percentage of the usable context window that triggers turn-end compaction.
    /// Zero disables turn-end compaction.
    pub model_post_turn_compact_threshold_percent: u8,

    /// Key into the model_providers map that specifies which provider to use.
    pub model_provider_id: String,

    /// Info needed to make an API request to the model.
    pub model_provider: ModelProviderInfo,

    /// Deprecated: `friendly` and `pragmatic` no longer select a style.
    pub personality: Option<Personality>,

    /// Whether the model may request a login shell for shell-based tools.
    /// Default to `true`
    ///
    /// If `true`, the model may request a login shell (`login = true`), and
    /// omitting `login` defaults to using a login shell.
    /// If `false`, the model can never use a login shell: `login = true`
    /// requests are rejected, and omitting `login` defaults to a non-login
    /// shell.
    pub allow_login_shell: bool,

    /// Policy used to build process environments for shell/unified exec.
    pub shell_environment_policy: ShellEnvironmentPolicy,

    /// When `true`, `AgentReasoning` events emitted by the backend will be
    /// suppressed from the frontend output. This can reduce visual noise when
    /// users are only interested in the final agent responses.
    pub hide_agent_reasoning: bool,

    /// When set to `true`, `AgentReasoningRawContentEvent` events will be shown in the UI/output.
    /// Defaults to `false`.
    pub show_raw_agent_reasoning: bool,

    /// Base instructions override.
    pub base_instructions: Option<String>,

    /// Origin of the configured base instructions when supplied by another session or lockfile.
    pub base_instructions_provenance: Option<BaseInstructionsProvenance>,

    /// Developer instructions override injected as a separate message.
    pub developer_instructions: Option<String>,

    /// Whether to inject the `<collaboration_mode>` developer block.
    pub include_collaboration_mode_instructions: bool,

    /// Whether to inject the `<skills_instructions>` developer block.
    pub include_skill_instructions: bool,

    /// Optional token budget override for the available-skills catalog.
    pub skill_max_context_tokens: Option<NonZeroUsize>,

    /// Whether to inject the `<environment_context>` user block.
    pub include_environment_context: bool,

    /// Whether environment context includes the current date and timezone.
    pub include_environment_context_time: bool,

    /// Compact prompt override.
    pub compact_prompt: Option<String>,

    /// Optional external notifier command. When set, Codex will spawn this
    /// program after each completed *turn* (i.e. when the agent finishes
    /// processing a user submission). The value must be the full command
    /// broken into argv tokens **without** the trailing JSON argument - Codex
    /// appends one extra argument containing a JSON payload describing the
    /// event.
    ///
    /// Example `~/.codex/config.toml` snippet:
    ///
    /// ```toml
    /// notify = ["notify-send", "Codex"]
    /// ```
    ///
    /// which will be invoked as:
    ///
    /// ```shell
    /// notify-send Codex '{"type":"agent-turn-complete","turn-id":"12345"}'
    /// ```
    ///
    /// If unset the feature is disabled.
    pub notify: Option<Vec<String>>,

    /// The absolute directory that should be treated as the current working
    /// directory for the session. All relative paths inside the business-logic
    /// layer are resolved against this path.
    pub cwd: AbsolutePathBuf,

    /// Absolute runtime workspace roots for the session.
    pub workspace_roots: Vec<AbsolutePathBuf>,
    /// Whether runtime workspace roots were supplied explicitly by the caller
    /// rather than defaulting to `cwd`.
    pub workspace_roots_explicit: bool,

    /// Preferred store for CLI auth credentials.
    /// file (default): Use a file in the Codex home directory.
    /// keyring: Use an OS-specific keyring service.
    /// auto: Use the OS-specific keyring service if available, otherwise use a file.
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,

    /// Definition for MCP servers that Codex can reach out to for tool calls.
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// IdP shared by all EMA MCP registrations.
    pub mcp_enterprise_managed_auth: Option<McpEnterpriseManagedAuthConfig>,

    /// When present, only these MCP servers omit the legacy `mcp__` namespace prefix.
    pub non_prefixed_mcp_tool_servers: Option<Vec<String>>,

    /// Preferred store for MCP OAuth credentials.
    /// keyring: Use an OS-specific keyring service.
    ///          Credentials stored in the keyring will only be readable by Codex unless the user explicitly grants access via OS-level keyring access.
    ///          https://github.com/openai/codex/blob/main/codex-rs/rmcp-client/src/oauth.rs#L2
    /// file: CODEX_HOME/.credentials.json
    ///       This file will be readable to Codex and other applications running as the same user.
    /// auto (default): keyring if available, otherwise file.
    pub mcp_oauth_credentials_store_mode: OAuthCredentialsStoreMode,

    /// Optional fixed port to use for the local HTTP callback server used during MCP OAuth login.
    ///
    /// When unset, Codex will bind to an ephemeral port chosen by the OS.
    pub mcp_oauth_callback_port: Option<u16>,

    /// Optional redirect URI to use during MCP OAuth login.
    ///
    /// When set, this URI is used in the OAuth authorization request instead
    /// of the local listener address. The local callback listener still binds
    /// to 127.0.0.1 (using `mcp_oauth_callback_port` when provided).
    pub mcp_oauth_callback_url: Option<String>,

    /// How long to wait for optional MCP servers while building the initial tool catalog.
    pub mcp_optional_startup_grace: Duration,

    /// Combined provider map (defaults plus user-defined providers).
    pub model_providers: HashMap<String, ModelProviderInfo>,

    /// Maximum total bytes of project instruction content across all selected environments.
    pub project_doc_max_bytes: usize,

    /// Additional filenames to try when looking for project-level docs.
    pub project_doc_fallback_filenames: Vec<String>,

    /// Token budget applied when storing tool/function outputs in the context manager.
    pub tool_output_token_limit: Option<usize>,

    /// Whether multi-agent tools are enabled through `[agents]`.
    pub agents_enabled: bool,

    /// User-configured maximum number of spawned agent threads per session.
    pub agent_max_threads: Option<usize>,

    /// Default model for spawned subagents when the spawn call does not select one.
    pub agent_default_subagent_model: Option<String>,

    /// Default reasoning effort for spawned subagents when the spawn call does not select one.
    pub agent_default_subagent_reasoning_effort: Option<ReasoningEffort>,

    /// Whether to record a model-visible message when an agent turn is interrupted.
    pub agent_interrupt_message_enabled: bool,

    /// Maximum nesting depth for V1 agent threads. Ignored by V2.
    pub agent_max_depth: i32,

    /// User-defined role declarations keyed by role name.
    pub agent_roles: BTreeMap<String, AgentRoleConfig>,

    /// Maximum token budget allowed for a goal and default budget for new goals.
    pub max_goal_token_budget: Option<i64>,

    /// Memories subsystem settings.
    pub memories: MemoriesConfig,

    /// Directory containing all Codex state (defaults to `~/.codex` but can be
    /// overridden by the `CODEX_HOME` environment variable).
    pub codex_home: AbsolutePathBuf,

    /// Resolved configuration shared by all Codex SQLite databases.
    pub sqlite: codex_state::SqliteConfig,

    /// Directory where Codex writes log files (defaults to `$CODEX_HOME/log`).
    pub log_dir: PathBuf,

    /// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
    pub history: History,

    /// When true, session is not persisted on disk. Default to `false`
    pub ephemeral: bool,

    /// Optional extra configuration fields for the thread.
    pub extra_config: Option<ExtraConfig>,

    /// Path to the current Codex executable. This cannot be set in the config
    /// file: it must be set in code via [`ConfigOverrides`].
    pub codex_self_exe: Option<PathBuf>,

    /// Value to use for `reasoning.effort` when making a request using the
    /// Responses API.
    pub model_reasoning_effort: Option<ReasoningEffort>,
    /// Optional Plan-mode-specific reasoning effort override used by the TUI.
    ///
    /// When unset, Plan mode uses the built-in Plan preset default (currently
    /// `medium`). When explicitly set (including `none`), this overrides the
    /// Plan preset. The `none` value means "no reasoning" (not "inherit the
    /// global default").
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,

    /// Optional value to use for `reasoning.summary` when making a request
    /// using the Responses API. When unset, the model catalog default is used.
    pub model_reasoning_summary: Option<ReasoningSummary>,

    /// Optional full model catalog loaded from `model_catalog_json`.
    /// When set, this replaces the bundled catalog for the current process.
    pub model_catalog: Option<ModelsResponse>,

    /// Optional verbosity control for GPT-5 models (Responses API `text.verbosity`).
    pub model_verbosity: Option<Verbosity>,

    /// Base URL for requests to ChatGPT (as opposed to the OpenAI API).
    pub chatgpt_base_url: String,

    /// Whether Codex-owned clients should respect host system proxy settings.
    pub respect_system_proxy: bool,

    /// Bounded, product-owned metadata attached to every Responses API request.
    pub responses_api_metadata: BTreeMap<String, String>,

    /// Experimental / do not use. Selects the thread persistence backend.
    pub experimental_thread_store: ThreadStoreConfig,
    /// When set, restricts ChatGPT login to one or more workspace identifiers.
    pub forced_chatgpt_workspace_id: Option<Vec<String>>,

    /// When set, restricts the login mechanism users may use.
    pub forced_login_method: Option<ForcedLoginMethod>,

    /// Explicit or feature-derived web search mode.
    pub web_search_mode: WebSearchMode,

    /// Additional parameters for the web search tool when it is enabled.
    pub web_search_config: Option<WebSearchConfig>,

    /// Whether to register the experimental request_user_input tool.
    pub experimental_request_user_input_enabled: bool,

    /// Whether to register the update_plan tool.
    pub update_plan_enabled: bool,

    /// Policy for collecting and validating tool runtimes.
    pub tool_registry: ToolRegistryConfig,

    /// Maximum poll window for background terminal output (`write_stdin`), in milliseconds.
    /// Default: `300000` (5 minutes).
    pub background_terminal_max_timeout: u64,

    /// Idle timeout for unsubscribed app-server threads, resolved at server startup.
    pub thread_unload_delay: Duration,

    /// Compatibility-only settings retained for legacy `ghost_snapshot`
    /// config loading.
    pub ghost_snapshot: GhostSnapshotConfig,

    /// Settings specific to the task-path-based multi-agent tool surface.
    pub multi_agent_v2: MultiAgentV2Config,

    /// Context-window token budget configuration, when enabled.
    pub token_budget: Option<TokenBudgetConfig>,
    /// Runtime snapshot of configured token-budget preferences before startup activation.
    pub token_budget_startup_config: Option<TokenBudgetStartupConfig>,
    /// Shared token budget for the root thread and its sub-agents.
    pub rollout_budget: Option<RolloutBudgetConfig>,
    /// Current-time reminder and clock tool configuration, when enabled.
    pub current_time_reminder: Option<CurrentTimeReminderConfig>,
    /// How the sleep tool is selected when its feature gate is enabled.
    pub sleep_tool_mode: SleepToolMode,

    /// Centralized feature flags; source of truth for feature gating.
    pub features: Features,

    /// Host feature defaults retained beneath explicit configuration overrides.
    pub runtime_feature_defaults: BTreeMap<Feature, bool>,

    /// When `true`, suppress warnings about unstable (under development) features.
    pub suppress_unstable_features_warning: bool,

    /// When `false`, disables OpenAI analytics across Codex product surfaces on this machine.
    /// Custom OTLP metrics exporters are controlled by `otel.metrics_exporter`.
    /// Voluntarily left as Optional because the default value might depend on the client.
    pub analytics_enabled: Option<bool>,

    /// OTEL configuration (exporter type, endpoint, headers, etc.).
    pub otel: codex_config::types::OtelConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolRegistryConfig {
    /// Fail the turn when multiple tools share the same effective name.
    pub error_on_tool_collisions: bool,
    /// Include authoritative tool information in per-turn request metadata.
    pub turn_metadata_includes_tool_info: bool,
}

const TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES: usize = 2000;
const TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES: usize = 2000;
const AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenBudgetConfig {
    pub use_history_notes_extension: bool,
    pub reminder_threshold_tokens: Option<i64>,
    pub reminder_message_template: String,
    pub guidance_message: Option<String>,
    pub auto_compact_fallback_prompt: Option<String>,
    pub auto_compact_fallback_buffer_tokens: Option<i64>,
}

impl TokenBudgetConfig {
    pub(crate) fn validate(&self) -> std::io::Result<()> {
        if self
            .reminder_threshold_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_threshold_tokens must be positive",
            ));
        }

        if self.reminder_message_template.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.reminder_message_template must not be empty",
            ));
        }
        if self.reminder_message_template.len() > TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.reminder_message_template must not exceed {TOKEN_BUDGET_REMINDER_MESSAGE_TEMPLATE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .guidance_message
            .as_ref()
            .is_some_and(|message| message.len() > TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.guidance_message must not exceed {TOKEN_BUDGET_GUIDANCE_MESSAGE_MAX_BYTES} bytes"
                ),
            ));
        }

        if self
            .auto_compact_fallback_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.len() > AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.token_budget.auto_compact_fallback_prompt must not exceed {AUTO_COMPACT_FALLBACK_PROMPT_MAX_BYTES} bytes"
                ),
            ));
        }
        if self.auto_compact_fallback_prompt.is_some()
            && self.auto_compact_fallback_buffer_tokens.is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens is required when auto_compact_fallback_prompt is set",
            ));
        }
        if self
            .auto_compact_fallback_buffer_tokens
            .is_some_and(|tokens| tokens <= 0)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.token_budget.auto_compact_fallback_buffer_tokens must be positive",
            ));
        }

        Ok(())
    }

    pub(crate) fn fallback_buffer_tokens(&self) -> i64 {
        if self.auto_compact_fallback_prompt.is_some() {
            self.auto_compact_fallback_buffer_tokens.unwrap_or(0)
        } else {
            0
        }
    }
}

impl Default for TokenBudgetConfig {
    fn default() -> Self {
        Self {
            use_history_notes_extension: false,
            reminder_threshold_tokens: None,
            reminder_message_template: ResolvedModelMessages::bundled()
                .token_budget_reminder_template()
                .to_owned(),
            guidance_message: None,
            auto_compact_fallback_prompt: None,
            auto_compact_fallback_buffer_tokens: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RolloutBudgetConfig {
    pub limit_tokens: i64,
    pub reminder_at_remaining_tokens: Vec<i64>,
    pub sampling_token_weight: f64,
    pub prefill_token_weight: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CurrentTimeReminderConfig {
    pub reminder_interval_seconds: u64,
    pub clock_source: CurrentTimeSource,
    pub delivery_mode: CurrentTimeReminderDeliveryMode,
    /// Whether to expose the input-interruptible `clock.sleep` tool.
    pub sleep_tool: bool,
}

impl Default for CurrentTimeReminderConfig {
    fn default() -> Self {
        Self {
            reminder_interval_seconds: 1,
            clock_source: CurrentTimeSource::System,
            delivery_mode: CurrentTimeReminderDeliveryMode::AnyInference,
            sleep_tool: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MultiAgentV2Config {
    pub max_concurrent_threads_per_session: usize,
    pub min_wait_timeout_ms: i64,
    pub max_wait_timeout_ms: i64,
    pub default_wait_timeout_ms: i64,
    pub usage_hint_text: Option<String>,
    pub root_agent_usage_hint_text: Option<String>,
    pub subagent_usage_hint_text: Option<String>,
    pub subagent_developer_instructions: Option<String>,
    pub preserve_fork_prefix: bool,
    pub multi_agent_mode_hint_text: Option<String>,
    pub tool_namespace: Option<String>,
    pub hide_spawn_agent_metadata: bool,
    pub expose_spawn_agent_model_overrides: bool,
    pub wait_agent_enabled: bool,
    pub disable_direct_message: bool,
    pub message_board_in_memory: bool,
    pub message_board_remote: Option<codex_features::RemoteMessageBoardConfigToml>,
}

impl MultiAgentV2Config {
    fn defaults_for_max_concurrency(max_concurrent_threads_per_session: usize) -> Self {
        Self {
            max_concurrent_threads_per_session,
            min_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MIN_WAIT_TIMEOUT_MS,
            max_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_MAX_WAIT_TIMEOUT_MS,
            default_wait_timeout_ms: DEFAULT_MULTI_AGENT_V2_DEFAULT_WAIT_TIMEOUT_MS,
            usage_hint_text: None,
            root_agent_usage_hint_text: None,
            subagent_usage_hint_text: None,
            subagent_developer_instructions: None,
            preserve_fork_prefix: false,
            multi_agent_mode_hint_text: None,
            tool_namespace: Some(DEFAULT_MULTI_AGENT_V2_TOOL_NAMESPACE.to_string()),
            hide_spawn_agent_metadata: true,
            expose_spawn_agent_model_overrides: true,
            wait_agent_enabled: true,
            disable_direct_message: false,
            message_board_in_memory: false,
            message_board_remote: None,
        }
    }
}

impl Default for MultiAgentV2Config {
    fn default() -> Self {
        Self::defaults_for_max_concurrency(
            DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION,
        )
    }
}

impl AuthManagerConfig for Config {
    fn codex_home(&self) -> PathBuf {
        self.codex_home.to_path_buf()
    }

    fn cli_auth_credentials_store_mode(&self) -> AuthCredentialsStoreMode {
        self.cli_auth_credentials_store_mode
    }

    fn auth_keyring_backend_kind(&self) -> AuthKeyringBackendKind {
        Config::auth_keyring_backend_kind(self)
    }

    fn forced_login_method(&self) -> Option<ForcedLoginMethod> {
        self.forced_login_method
    }

    fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id.clone()
    }

    fn chatgpt_base_url(&self) -> String {
        self.chatgpt_base_url.clone()
    }

    fn auth_route_config(&self) -> AuthRouteConfig {
        Config::auth_route_config(self)
    }
}

#[derive(Clone, Default)]
pub struct ConfigBuilder {
    codex_home: Option<PathBuf>,
    cli_overrides: Option<Vec<(String, TomlValue)>>,
    harness_overrides: Option<ConfigOverrides>,
    loader_overrides: Option<LoaderOverrides>,
    strict_config: bool,
    thread_config_loader: Option<Arc<dyn ThreadConfigLoader>>,
    fallback_cwd: Option<PathBuf>,
    without_project_context: bool,
}

impl ConfigBuilder {
    pub fn codex_home(mut self, codex_home: PathBuf) -> Self {
        self.codex_home = Some(codex_home);
        self
    }

    pub fn cli_overrides(mut self, cli_overrides: Vec<(String, TomlValue)>) -> Self {
        self.cli_overrides = Some(cli_overrides);
        self
    }

    pub fn harness_overrides(mut self, harness_overrides: ConfigOverrides) -> Self {
        self.harness_overrides = Some(harness_overrides);
        self
    }

    pub fn loader_overrides(mut self, loader_overrides: LoaderOverrides) -> Self {
        self.loader_overrides = Some(loader_overrides);
        self
    }

    pub fn strict_config(mut self, strict_config: bool) -> Self {
        self.strict_config = strict_config;
        self
    }

    pub fn thread_config_loader(
        mut self,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) -> Self {
        self.thread_config_loader = Some(thread_config_loader);
        self
    }

    pub fn fallback_cwd(mut self, fallback_cwd: Option<PathBuf>) -> Self {
        self.fallback_cwd = fallback_cwd;
        self
    }

    /// Materializes `Config.cwd` without using it as config-layer context.
    pub fn without_project_context(mut self) -> Self {
        self.without_project_context = true;
        self
    }

    pub async fn build(self) -> std::io::Result<Config> {
        // Keep the large config-loading future off small runtime thread stacks.
        Box::pin(self.build_inner()).await
    }

    async fn build_inner(self) -> std::io::Result<Config> {
        let Self {
            codex_home,
            cli_overrides,
            harness_overrides,
            loader_overrides,
            strict_config,
            thread_config_loader,
            fallback_cwd,
            without_project_context,
        } = self;
        let codex_home = match codex_home {
            Some(codex_home) => AbsolutePathBuf::from_absolute_path(codex_home)?,
            None => find_codex_home()?,
        };
        let cli_overrides = cli_overrides.unwrap_or_default();
        let mut harness_overrides = harness_overrides.unwrap_or_default();
        let loader_overrides = loader_overrides.unwrap_or_default();
        let cwd_override = harness_overrides.cwd.as_deref().or(fallback_cwd.as_deref());
        let cwd = match cwd_override {
            Some(path) => AbsolutePathBuf::relative_to_current_dir(path)?,
            None => AbsolutePathBuf::current_dir()?,
        };
        harness_overrides.cwd = Some(cwd.to_path_buf());
        let config_layer_stack = load_config_layers_state(
            LOCAL_FS.as_ref(),
            &codex_home,
            (!without_project_context).then_some(cwd),
            &cli_overrides,
            ConfigLoadOptions {
                loader_overrides,
                strict_config,
            },
            thread_config_loader
                .as_deref()
                .unwrap_or(&codex_config::NoopThreadConfigLoader),
        )
        .await?;
        let config_toml = config_toml_from_layers(&config_layer_stack).await?;
        Config::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            config_toml,
            harness_overrides,
            codex_home,
            config_layer_stack,
        )
        .await
    }
}

async fn config_toml_from_layers(layers: &ConfigLayerStack) -> std::io::Result<ConfigToml> {
    // The loader resolves paths relative to each layer's file before deserialization.
    match layers.effective_config().try_into() {
        Ok(config_toml) => Ok(config_toml),
        Err(err) => {
            if let Some(config_error) = codex_config::first_layer_config_error::<ConfigToml>(
                layers,
                codex_config::CONFIG_TOML_FILE,
            )
            .await
            {
                return Err(codex_config::io_error_from_config_error(
                    std::io::ErrorKind::InvalidData,
                    config_error,
                    Some(err),
                ));
            }
            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
        }
    }
}

impl Config {
    pub fn sqlite_config(&self) -> &codex_state::SqliteConfig {
        &self.sqlite
    }

    pub(crate) fn multi_agent_version_override(&self) -> Option<MultiAgentVersion> {
        if self.features.enabled(Feature::MultiAgentV2) {
            Some(MultiAgentVersion::V2)
        } else if !self.agents_enabled {
            Some(MultiAgentVersion::Disabled)
        } else {
            None
        }
    }

    pub(crate) fn multi_agent_version_from_features(&self) -> MultiAgentVersion {
        self.multi_agent_version_override().unwrap_or_else(|| {
            if self.features.enabled(Feature::Collab) {
                MultiAgentVersion::V1
            } else {
                MultiAgentVersion::Disabled
            }
        })
    }

    pub(crate) fn multi_agent_version_for_model(
        &self,
        model_multi_agent_version: Option<MultiAgentVersion>,
    ) -> MultiAgentVersion {
        self.multi_agent_version_override()
            .or(model_multi_agent_version)
            .unwrap_or_else(|| self.multi_agent_version_from_features())
    }

    pub(crate) fn effective_agent_max_threads(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> Option<usize> {
        match multi_agent_version {
            MultiAgentVersion::V2 => Some(
                self.multi_agent_v2
                    .max_concurrent_threads_per_session
                    .saturating_sub(1),
            ),
            MultiAgentVersion::Disabled | MultiAgentVersion::V1 => {
                self.agent_max_threads.or(DEFAULT_AGENT_MAX_THREADS)
            }
        }
    }

    /// Runtime workspace roots as URIs, without interpreting them on the current host.
    pub fn effective_workspace_roots(&self) -> Vec<PathUri> {
        let mut workspace_roots = self
            .workspace_roots
            .iter()
            .map(PathUri::from_abs_path)
            .collect::<Vec<_>>();
        // Preserve spelling changes even when Windows path comparison folds case.
        let mut seen = HashSet::new();
        workspace_roots.retain(|root| seen.insert(root.to_string()));
        workspace_roots
    }

    pub fn to_models_manager_config(&self) -> ModelsManagerConfig {
        ModelsManagerConfig {
            model_context_window: self.model_context_window,
            model_auto_compact_token_limit: self.model_auto_compact_token_limit,
            tool_output_token_limit: self.tool_output_token_limit,
            base_instructions: self.base_instructions.clone().filter(|_| {
                !matches!(
                    self.base_instructions_provenance,
                    Some(BaseInstructionsProvenance::Model { .. })
                )
            }),
            personality: self.personality,
            model_catalog: self.model_catalog.clone(),
        }
    }

    /// Returns auth routing resolved from the effective feature configuration.
    pub fn auth_route_config(&self) -> AuthRouteConfig {
        self.application_auth_route_config
            .clone()
            .unwrap_or_else(|| {
                AuthRouteConfig::from_http_client_factory(self.http_client_factory())
            })
    }

    /// Creates the HTTP client factory resolved from the effective feature configuration.
    pub fn http_client_factory(&self) -> HttpClientFactory {
        let outbound_proxy_policy = if self.respect_system_proxy {
            OutboundProxyPolicy::RespectSystemProxy
        } else {
            OutboundProxyPolicy::ReqwestDefault
        };
        let mut factory = HttpClientFactory::new(outbound_proxy_policy);
        if !self.respect_system_proxy && self.features.enabled(Feature::SystemProxyFallback) {
            factory = factory.with_system_proxy_fallback();
        }
        if self.features.enabled(Feature::Psp) {
            factory.with_chatgpt_cookies([HeaderValue::from_static("oai-chat-psp=true")])
        } else {
            factory
        }
    }

    pub fn to_mcp_config(&self) -> McpConfig {
        let mut catalog = ResolvedMcpCatalog::builder();
        if self.features.enabled(Feature::UseXaa)
            && let Some(auth) = &self.mcp_enterprise_managed_auth
        {
            catalog.enable_ema(auth.idp.clone());
        }
        for (name, server) in &self.mcp_servers {
            catalog.register(McpServerRegistration::from_config(
                name.clone(),
                server.clone(),
            ));
        }

        McpConfig {
            chatgpt_base_url: self.chatgpt_base_url.clone(),
            requires_read_only_mcp_tools: false,
            codex_home: self.codex_home.to_path_buf(),
            mcp_enterprise_managed_auth: self.mcp_enterprise_managed_auth.clone(),
            xaa_enabled: self.features.enabled(Feature::UseXaa)
                && self.mcp_enterprise_managed_auth.is_some(),
            mcp_oauth_credentials_store_mode: self.mcp_oauth_credentials_store_mode,
            oauth_refresh_mode: if self.features.enabled(Feature::McpOAuthRefreshCoordination) {
                McpOAuthRefreshMode::Coordinated
            } else {
                McpOAuthRefreshMode::Legacy
            },
            auth_keyring_backend_kind: self.auth_keyring_backend_kind(),
            mcp_oauth_callback_port: self.mcp_oauth_callback_port,
            mcp_oauth_callback_url: self.mcp_oauth_callback_url.clone(),
            optional_mcp_startup_grace: self.mcp_optional_startup_grace,
            skill_mcp_dependency_install_enabled: self
                .features
                .enabled(Feature::SkillMcpDependencyInstall),
            environment_cwds: HashMap::new(),
            prefix_mcp_tool_names: self.prefix_mcp_tool_names(),
            non_prefixed_mcp_tool_servers: if self
                .features
                .enabled(Feature::NonPrefixedMcpToolNames)
            {
                self.non_prefixed_mcp_tool_servers
                    .clone()
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            protocol_mode: self.mcp_protocol_mode(),
            // https://modelcontextprotocol.io/specification/2025-06-18/client/elicitation#capabilities
            // indicates this should be an empty object.
            client_elicitation_capability: ElicitationCapability::default(),
            mcp_server_catalog: catalog.build(),
        }
    }

    pub(crate) fn prefix_mcp_tool_names(&self) -> bool {
        !self.features.enabled(Feature::NonPrefixedMcpToolNames)
            || self.non_prefixed_mcp_tool_servers.is_some()
    }

    pub fn mcp_protocol_mode(&self) -> McpProtocolMode {
        if self.features.enabled(Feature::Mcp20260728) {
            McpProtocolMode::V20260728
        } else {
            McpProtocolMode::Legacy
        }
    }

    pub fn workspace_routing_context(&self) -> codex_model_provider::WorkspaceRoutingContext {
        codex_model_provider::WorkspaceRoutingContext::new(self.chatgpt_base_url.clone())
            .with_session(codex_login::WorkspaceRoutingSession {
                cwd: self.cwd.to_path_buf(),
                config_layer_stack: self.config_layer_stack.clone(),
            })
    }

    pub async fn rebuild_with_session_layers(
        session_layers: &ConfigLayerStack,
        cwd: PathBuf,
        refreshed_layers: &ConfigLayerStack,
        codex_home: AbsolutePathBuf,
    ) -> std::io::Result<Self> {
        let config_layer_stack =
            Self::layer_stack_preserving_session(session_layers, refreshed_layers)?;
        let cfg = config_toml_from_layers(&config_layer_stack).await?;
        Self::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            cfg,
            ConfigOverrides {
                cwd: Some(cwd),
                ..Default::default()
            },
            codex_home,
            config_layer_stack,
        )
        .await
    }

    fn layer_stack_preserving_session(
        session_layers: &ConfigLayerStack,
        refreshed_layers: &ConfigLayerStack,
    ) -> std::io::Result<ConfigLayerStack> {
        let mut layers = refreshed_layers
            .all_layers_low_to_high()
            .filter(|layer| !is_session_layer(&layer.name))
            .cloned()
            .collect::<Vec<_>>();
        layers.extend(
            session_layers
                .all_layers_low_to_high()
                .filter(|layer| is_session_layer(&layer.name))
                .cloned(),
        );
        layers.sort_by_key(|layer| layer.name.precedence());

        ConfigLayerStack::new(layers)
    }

    /// This is the preferred way to create an instance of [Config].
    pub async fn load_with_cli_overrides(
        cli_overrides: Vec<(String, TomlValue)>,
    ) -> std::io::Result<Self> {
        ConfigBuilder::default()
            .cli_overrides(cli_overrides)
            .build()
            .await
    }

    /// Load a default configuration when user config files are invalid.
    pub async fn load_default_with_cli_overrides(
        cli_overrides: Vec<(String, TomlValue)>,
    ) -> std::io::Result<Self> {
        let codex_home = find_codex_home()?;
        Self::load_default_with_cli_overrides_for_codex_home(
            codex_home.to_path_buf(),
            cli_overrides,
        )
        .await
    }

    /// Load a default configuration for a specific Codex home without reading
    /// user, project, or system config layers.
    pub async fn load_default_with_cli_overrides_for_codex_home(
        codex_home: PathBuf,
        cli_overrides: Vec<(String, TomlValue)>,
    ) -> std::io::Result<Self> {
        let mut merged = toml::Value::try_from(ConfigToml::default()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to serialize default config: {e}"),
            )
        })?;
        let cli_layer = codex_config::build_cli_overrides_layer(&cli_overrides);
        codex_config::merge_toml_values(&mut merged, &cli_layer);
        let codex_home = AbsolutePathBuf::from_absolute_path_checked(codex_home)?;
        let config_toml = deserialize_config_toml_with_base(merged, &codex_home)?;
        Self::load_config_with_layer_stack(
            LOCAL_FS.as_ref(),
            config_toml,
            ConfigOverrides::default(),
            codex_home,
            ConfigLayerStack::default(),
        )
        .await
    }
    /// This is a secondary way of creating [Config], which is appropriate when
    /// the harness supplies options that are not supported in [ConfigToml],
    /// such as `cwd` and `codex_self_exe` (see [ConfigOverrides]).
    pub async fn load_with_cli_overrides_and_harness_overrides(
        cli_overrides: Vec<(String, TomlValue)>,
        harness_overrides: ConfigOverrides,
    ) -> std::io::Result<Self> {
        ConfigBuilder::default()
            .cli_overrides(cli_overrides)
            .harness_overrides(harness_overrides)
            .build()
            .await
    }
}

pub fn resolve_profile_v2_config_path(
    codex_home: &Path,
    profile_name: &ProfileV2Name,
) -> AbsolutePathBuf {
    AbsolutePathBuf::resolve_path_against_base(
        format!("{profile_name}{CONFIG_PROFILE_V2_SUFFIX}"),
        codex_home,
    )
}

/// DEPRECATED: Use [Config::load_with_cli_overrides()] instead; [ConfigToml]
/// is only partially resolved config.
pub async fn load_config_as_toml_with_cli_overrides(
    codex_home: &Path,
    cwd: Option<&AbsolutePathBuf>,
    cli_overrides: Vec<(String, TomlValue)>,
    loader_overrides: LoaderOverrides,
) -> std::io::Result<ConfigToml> {
    load_config_as_toml_with_cli_and_loader_overrides(
        codex_home,
        cwd,
        cli_overrides,
        loader_overrides,
    )
    .await
}

/// DEPRECATED for most callers: prefer [Config::load_with_cli_overrides()] or
/// [ConfigBuilder]; [ConfigToml] is only partially resolved config.
pub async fn load_config_as_toml_with_cli_and_loader_overrides(
    codex_home: &Path,
    cwd: Option<&AbsolutePathBuf>,
    cli_overrides: Vec<(String, TomlValue)>,
    loader_overrides: LoaderOverrides,
) -> std::io::Result<ConfigToml> {
    load_config_as_toml_with_cli_and_load_options(codex_home, cwd, cli_overrides, loader_overrides)
        .await
}

/// DEPRECATED for most callers: prefer [Config::load_with_cli_overrides()] or
/// [ConfigBuilder]; [ConfigToml] is only partially resolved config.
pub async fn load_config_as_toml_with_cli_and_load_options(
    codex_home: &Path,
    cwd: Option<&AbsolutePathBuf>,
    cli_overrides: Vec<(String, TomlValue)>,
    options: impl Into<ConfigLoadOptions>,
) -> std::io::Result<ConfigToml> {
    load_config_toml_with_layer_stack(codex_home, cwd, cli_overrides, options)
        .await
        .map(|result| result.config_toml)
}

/// Partially loaded config plus the layer stack used to derive it.
///
/// This is intended for startup paths that must inspect raw config before a
/// full [`Config`] can be constructed.
pub struct ConfigTomlLoadResult {
    pub config_toml: ConfigToml,
    pub config_layer_stack: ConfigLayerStack,
}

/// Loads the partially merged config together with the layer stack used to
/// derive it, before constructing a full [`Config`].
pub async fn load_config_toml_with_layer_stack(
    codex_home: &Path,
    cwd: Option<&AbsolutePathBuf>,
    cli_overrides: Vec<(String, TomlValue)>,
    options: impl Into<ConfigLoadOptions>,
) -> std::io::Result<ConfigTomlLoadResult> {
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home,
        cwd.cloned(),
        &cli_overrides,
        options,
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let merged_toml = config_layer_stack.effective_config();
    let cfg = deserialize_config_toml_with_base(merged_toml, codex_home).map_err(|e| {
        tracing::error!("Failed to deserialize overridden config: {e}");
        e
    })?;

    Ok(ConfigTomlLoadResult {
        config_toml: cfg,
        config_layer_stack,
    })
}

pub fn deserialize_config_toml_with_base(
    root_value: TomlValue,
    config_base_dir: &Path,
) -> std::io::Result<ConfigToml> {
    // This guard ensures that any relative paths that is deserialized into an
    // [AbsolutePathBuf] is resolved against `config_base_dir`.
    let _guard = AbsolutePathBufGuard::new(config_base_dir);
    root_value
        .try_into()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn load_catalog_json(path: &AbsolutePathBuf) -> std::io::Result<ModelsResponse> {
    let file_contents = std::fs::read_to_string(path)?;
    let catalog = serde_json::from_str::<ModelsResponse>(&file_contents).map_err(|err| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "failed to parse model_catalog_json path `{}` as JSON: {err}",
                path.display()
            ),
        )
    })?;
    if catalog.models.is_empty() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "model_catalog_json path `{}` must contain at least one model",
                path.display()
            ),
        ));
    }
    Ok(catalog)
}

fn load_model_catalog(
    model_catalog_json: Option<AbsolutePathBuf>,
) -> std::io::Result<Option<ModelsResponse>> {
    model_catalog_json
        .map(|path| load_catalog_json(&path))
        .transpose()
}

pub async fn load_global_mcp_servers(
    codex_home: &Path,
) -> std::io::Result<BTreeMap<String, McpServerConfig>> {
    // In general, Config::load_with_cli_overrides() should be used to load the
    // full config with requirements.toml applied, but in this case, we need
    // access to the raw TOML in order to warn the user about deprecated fields.
    //
    // Note that a more precise way to do this would be to audit the individual
    // config layers for deprecated fields rather than reporting on the merged
    // result.
    let cli_overrides = Vec::<(String, TomlValue)>::new();
    // There is no cwd/project context for this query, so this will not include
    // MCP servers defined in in-repo .codex/ folders.
    let cwd: Option<AbsolutePathBuf> = None;
    let config_layer_stack = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home,
        cwd,
        &cli_overrides,
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;
    let merged_toml = config_layer_stack.effective_config();
    let Some(servers_value) = merged_toml.get("mcp_servers") else {
        return Ok(BTreeMap::new());
    };

    ensure_no_inline_bearer_tokens(servers_value)?;

    servers_value
        .clone()
        .try_into()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// We briefly allowed plain text bearer_token fields in MCP server configs.
/// We want to warn people who recently added these fields but can remove this after a few months.
fn ensure_no_inline_bearer_tokens(value: &TomlValue) -> std::io::Result<()> {
    let Some(servers_table) = value.as_table() else {
        return Ok(());
    };

    for (server_name, server_value) in servers_table {
        if let Some(server_table) = server_value.as_table()
            && server_table.contains_key("bearer_token")
        {
            let message = format!(
                "mcp_servers.{server_name} uses unsupported `bearer_token`; set `bearer_token_env_var`."
            );
            return Err(std::io::Error::new(ErrorKind::InvalidData, message));
        }
    }

    Ok(())
}

/// Save the default OSS provider preference to config.toml
pub fn set_default_oss_provider(codex_home: &Path, provider: &str) -> std::io::Result<()> {
    codex_config::config_toml::validate_oss_provider(provider)?;
    use toml_edit::value;

    let edits = [ConfigEdit::SetPath {
        segments: vec!["oss_provider".to_string()],
        value: value(provider),
    }];

    ConfigEditsBuilder::new(codex_home)
        .with_edits(edits)
        .apply_blocking()
        .map_err(|err| std::io::Error::other(format!("failed to persist config.toml: {err}")))
}

fn thread_store_config(thread_store: Option<ThreadStoreToml>) -> ThreadStoreConfig {
    match thread_store {
        Some(ThreadStoreToml::Local {}) => ThreadStoreConfig::Local,
        Some(ThreadStoreToml::InMemory { id }) => ThreadStoreConfig::InMemory { id },
        None => ThreadStoreConfig::Local,
    }
}

fn is_session_layer(source: &ConfigLayerSource) -> bool {
    matches!(source, ConfigLayerSource::SessionFlags)
}

/// Optional overrides for user configuration (e.g., from CLI flags).
#[derive(Default, Debug, Clone)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub review_model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub model_provider: Option<String>,
    pub service_tier: Option<Option<String>>,
    pub codex_self_exe: Option<PathBuf>,
    pub base_instructions: Option<String>,
    pub developer_instructions: Option<String>,
    /// Deprecated: `friendly` and `pragmatic` no longer select a style.
    pub personality: Option<Personality>,
    pub compact_prompt: Option<String>,
    pub show_raw_agent_reasoning: Option<bool>,
    pub tools_web_search_request: Option<bool>,
    pub ephemeral: Option<bool>,
    /// Explicit absolute runtime workspace roots for this session. When set,
    /// this is the full runtime root list rather than an additive override.
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
}

fn dedupe_absolute_paths(paths: &mut Vec<AbsolutePathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

/// Resolves the OSS provider from CLI override or global config.
/// Returns `None` if no provider is configured at any level.
pub fn resolve_oss_provider(
    explicit_provider: Option<&str>,
    config_toml: &ConfigToml,
) -> Option<String> {
    if let Some(provider) = explicit_provider {
        // Explicit provider specified (e.g., via --local-provider)
        Some(provider.to_string())
    } else {
        config_toml.oss_provider.clone()
    }
}

/// Resolve the web search mode from explicit config and feature flags.
fn resolve_web_search_mode(config_toml: &ConfigToml, features: &Features) -> Option<WebSearchMode> {
    if let Some(mode) = config_toml.web_search {
        return Some(mode);
    }
    if features.enabled(Feature::WebSearchCached) {
        return Some(WebSearchMode::Cached);
    }
    if features.enabled(Feature::WebSearchRequest) {
        return Some(WebSearchMode::Live);
    }
    None
}

fn resolve_web_search_config(config_toml: &ConfigToml) -> Option<WebSearchConfig> {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.web_search.as_ref())
        .cloned()
        .map(Into::into)
}

fn resolve_experimental_request_user_input_enabled(config_toml: &ConfigToml) -> bool {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.experimental_request_user_input.as_ref())
        .is_none_or(|config| config.enabled)
}

fn resolve_update_plan_enabled(config_toml: &ConfigToml) -> bool {
    config_toml
        .tools
        .as_ref()
        .and_then(|tools| tools.update_plan.as_ref())
        .is_some_and(|config| config.enabled)
}

fn resolve_multi_agent_v2_config(config_toml: &ConfigToml) -> MultiAgentV2Config {
    let base = multi_agent_v2_toml_config(config_toml.features.as_ref());
    let max_concurrent_threads_per_session = base
        .and_then(|config| config.max_concurrent_threads_per_session)
        .or_else(|| {
            config_toml
                .agents
                .as_ref()
                .and_then(|agents| agents.max_concurrent_threads_per_session)
                .map(|max_threads| max_threads.saturating_add(1))
        })
        .unwrap_or(DEFAULT_MULTI_AGENT_V2_MAX_CONCURRENT_THREADS_PER_SESSION);
    let default =
        MultiAgentV2Config::defaults_for_max_concurrency(max_concurrent_threads_per_session);
    let min_wait_timeout_ms = base
        .and_then(|config| config.min_wait_timeout_ms)
        .unwrap_or(default.min_wait_timeout_ms);
    let max_wait_timeout_ms = base
        .and_then(|config| config.max_wait_timeout_ms)
        .unwrap_or(default.max_wait_timeout_ms);
    let default_wait_timeout_ms = base
        .and_then(|config| config.default_wait_timeout_ms)
        .unwrap_or(default.default_wait_timeout_ms);
    let usage_hint_text = base
        .and_then(|config| config.usage_hint_text.as_ref())
        .cloned()
        .or(default.usage_hint_text);
    let hide_spawn_agent_metadata = base
        .and_then(|config| config.hide_spawn_agent_metadata)
        .unwrap_or(default.hide_spawn_agent_metadata);
    let expose_spawn_agent_model_overrides = base
        .and_then(|config| config.expose_spawn_agent_model_overrides)
        .unwrap_or(default.expose_spawn_agent_model_overrides);
    let root_agent_usage_hint_text = base
        .and_then(|config| config.root_agent_usage_hint_text.as_ref())
        .cloned();
    let subagent_usage_hint_text = base
        .and_then(|config| config.subagent_usage_hint_text.as_ref())
        .cloned();
    let wait_agent_enabled = base
        .and_then(|config| config.wait_agent_enabled)
        .unwrap_or(default.wait_agent_enabled);
    let disable_direct_message = base
        .and_then(|config| config.disable_direct_message)
        .unwrap_or(default.disable_direct_message);
    let message_board_in_memory = base
        .and_then(|config| config.message_board_in_memory)
        .unwrap_or(default.message_board_in_memory);
    let subagent_developer_instructions = base
        .and_then(|config| config.subagent_developer_instructions.as_ref())
        .map(|instructions| instructions.trim().to_string());
    let preserve_fork_prefix = base
        .and_then(|config| config.preserve_fork_prefix)
        .unwrap_or(default.preserve_fork_prefix);
    let multi_agent_mode_hint_text = base
        .and_then(|config| config.multi_agent_mode_hint_text.as_ref())
        .cloned()
        .or(default.multi_agent_mode_hint_text);
    let tool_namespace = base
        .and_then(|config| config.tool_namespace.as_ref())
        .cloned()
        .or(default.tool_namespace);

    MultiAgentV2Config {
        max_concurrent_threads_per_session,
        min_wait_timeout_ms,
        max_wait_timeout_ms,
        default_wait_timeout_ms,
        usage_hint_text,
        root_agent_usage_hint_text,
        subagent_usage_hint_text,
        subagent_developer_instructions,
        preserve_fork_prefix,
        multi_agent_mode_hint_text,
        tool_namespace,
        hide_spawn_agent_metadata,
        expose_spawn_agent_model_overrides,
        wait_agent_enabled,
        disable_direct_message,
        message_board_in_memory,
        message_board_remote: base.and_then(|config| config.message_board_remote.clone()),
    }
}

pub(crate) fn resolve_token_budget_config(
    config_toml: &ConfigToml,
    features: &Features,
) -> std::io::Result<Option<TokenBudgetConfig>> {
    if !features.enabled(Feature::TokenBudget) {
        return Ok(None);
    }

    let token_budget_config = token_budget_toml_config(config_toml.features.as_ref());
    let use_history_notes_extension = token_budget_config
        .and_then(|config| config.use_history_notes_extension)
        .unwrap_or_default();
    let reminder_threshold_tokens =
        token_budget_config.and_then(|config| config.reminder_threshold_tokens);
    let reminder_message_template = token_budget_config
        .and_then(|config| config.reminder_message_template.clone())
        .unwrap_or_else(|| {
            ResolvedModelMessages::bundled()
                .token_budget_reminder_template()
                .to_owned()
        });
    let guidance_message = token_budget_config
        .and_then(|config| config.guidance_message.clone())
        .filter(|message| !message.trim().is_empty());
    let auto_compact_fallback_prompt = token_budget_config
        .and_then(|config| config.auto_compact_fallback_prompt.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let auto_compact_fallback_buffer_tokens =
        token_budget_config.and_then(|config| config.auto_compact_fallback_buffer_tokens);

    let token_budget = TokenBudgetConfig {
        use_history_notes_extension,
        reminder_threshold_tokens,
        reminder_message_template,
        guidance_message,
        auto_compact_fallback_prompt,
        auto_compact_fallback_buffer_tokens,
    };
    token_budget.validate()?;
    Ok(Some(token_budget))
}

fn resolve_rollout_budget_config(
    config_toml: &ConfigToml,
    features: &Features,
) -> std::io::Result<Option<RolloutBudgetConfig>> {
    if !features.enabled(Feature::RolloutBudget) {
        return Ok(None);
    }
    let missing_limit_error = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.limit_tokens is required when rollout_budget is enabled",
        )
    };
    let Some(FeatureToml::Config(config)) = config_toml
        .features
        .as_ref()
        .and_then(|features| features.rollout_budget.as_ref())
    else {
        return Err(missing_limit_error());
    };
    let Some(limit_tokens) = config.limit_tokens else {
        return Err(missing_limit_error());
    };
    if limit_tokens <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.limit_tokens must be positive",
        ));
    }
    let reminder_at_remaining_tokens =
        config
            .reminder_at_remaining_tokens
            .clone()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "features.rollout_budget.reminder_at_remaining_tokens is required when rollout_budget is enabled",
                )
            })?;
    if reminder_at_remaining_tokens
        .iter()
        .any(|&tokens| tokens <= 0 || tokens >= limit_tokens)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "features.rollout_budget.reminder_at_remaining_tokens must contain only positive values below limit_tokens",
        ));
    }
    let sampling_token_weight = config.sampling_token_weight.unwrap_or(1.0);
    let prefill_token_weight = config.prefill_token_weight.unwrap_or(1.0);
    for (field, weight) in [
        ("sampling_token_weight", sampling_token_weight),
        ("prefill_token_weight", prefill_token_weight),
    ] {
        if !weight.is_finite() || weight < 0.0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("features.rollout_budget.{field} must be finite and non-negative"),
            ));
        }
    }
    Ok(Some(RolloutBudgetConfig {
        limit_tokens,
        reminder_at_remaining_tokens,
        sampling_token_weight,
        prefill_token_weight,
    }))
}

fn resolve_current_time_reminder_config(
    config_toml: &ConfigToml,
    features: &Features,
) -> std::io::Result<Option<CurrentTimeReminderConfig>> {
    if !features.enabled(Feature::CurrentTimeReminder) {
        return Ok(None);
    }

    let base = current_time_reminder_toml_config(config_toml.features.as_ref());
    let default = CurrentTimeReminderConfig::default();
    let reminder_interval_seconds = base
        .and_then(|config| config.reminder_interval_seconds)
        .unwrap_or(default.reminder_interval_seconds);

    Ok(Some(CurrentTimeReminderConfig {
        reminder_interval_seconds,
        clock_source: base
            .and_then(|config| config.clock_source)
            .unwrap_or(default.clock_source),
        delivery_mode: base
            .and_then(|config| config.delivery_mode)
            .unwrap_or(default.delivery_mode),
        sleep_tool: base
            .and_then(|config| config.sleep_tool)
            .unwrap_or(default.sleep_tool),
    }))
}

fn multi_agent_v2_toml_config(features: Option<&FeaturesToml>) -> Option<&MultiAgentV2ConfigToml> {
    match features?.multi_agent_v2.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

fn token_budget_toml_config(features: Option<&FeaturesToml>) -> Option<&TokenBudgetConfigToml> {
    match features?.token_budget.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

fn current_time_reminder_toml_config(
    features: Option<&FeaturesToml>,
) -> Option<&CurrentTimeReminderConfigToml> {
    match features?.current_time_reminder.as_ref()? {
        FeatureToml::Enabled(_) => None,
        FeatureToml::Config(config) => Some(config),
    }
}

/// Bootstrap-only resolver for startup work that runs before final [`Config`] loading.
pub fn resolve_bootstrap_respect_system_proxy(cfg: &ConfigToml) -> bool {
    resolve_bootstrap_http_client_factory(cfg).outbound_proxy_policy()
        == OutboundProxyPolicy::RespectSystemProxy
}

/// Resolves auth route settings for startup work that runs before final [`Config`] loading.
pub fn resolve_bootstrap_auth_route_config(cfg: &ConfigToml) -> AuthRouteConfig {
    AuthRouteConfig::from_http_client_factory(resolve_bootstrap_http_client_factory(cfg))
}

/// Resolves shared HTTP routing for startup work that runs before final [`Config`] loading.
pub fn resolve_bootstrap_http_client_factory(cfg: &ConfigToml) -> HttpClientFactory {
    let features = Features::from_sources(
        FeatureConfigSource {
            features: cfg.features.as_ref(),
        },
        FeatureConfigSource::default(),
        FeatureOverrides::default(),
    );
    let outbound_proxy_policy = if features.enabled(Feature::RespectSystemProxy) {
        OutboundProxyPolicy::RespectSystemProxy
    } else {
        OutboundProxyPolicy::ReqwestDefault
    };
    let mut factory = HttpClientFactory::new(outbound_proxy_policy);
    if outbound_proxy_policy == OutboundProxyPolicy::ReqwestDefault
        && features.enabled(Feature::SystemProxyFallback)
    {
        factory = factory.with_system_proxy_fallback();
    }
    factory
}

/// Picks the web search mode for a turn. Without sandboxing, live search is preferred over the
/// cached index unless config explicitly selected `disabled` or `indexed`.
pub(crate) fn resolve_web_search_mode_for_turn(
    preferred: WebSearchMode,
    provider_capabilities: ProviderCapabilities,
) -> WebSearchMode {
    let is_allowed = |mode: WebSearchMode| match mode {
        WebSearchMode::Live | WebSearchMode::Indexed => provider_capabilities.external_web_access,
        WebSearchMode::Cached | WebSearchMode::Disabled => true,
    };
    let candidates = if matches!(preferred, WebSearchMode::Disabled | WebSearchMode::Indexed) {
        vec![
            preferred,
            WebSearchMode::Cached,
            WebSearchMode::Live,
            WebSearchMode::Disabled,
        ]
    } else {
        vec![
            WebSearchMode::Live,
            WebSearchMode::Cached,
            WebSearchMode::Disabled,
        ]
    };
    candidates
        .into_iter()
        .find(|mode| is_allowed(*mode))
        .unwrap_or(WebSearchMode::Disabled)
}

fn validate_multi_agent_v2_wait_timeout(label: &str, value: i64) -> std::io::Result<()> {
    if value < HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} must be at least {HARD_MIN_MULTI_AGENT_V2_TIMEOUT_MS}"),
        ));
    }
    if value > HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} must be at most {HARD_MAX_MULTI_AGENT_V2_TIMEOUT_MS}"),
        ));
    }
    Ok(())
}

fn validate_multi_agent_v2_tool_namespace(namespace: Option<&str>) -> std::io::Result<()> {
    const LABEL: &str = "features.multi_agent_v2.tool_namespace";
    const MAX_LEN: usize = 64;
    const RESERVED_RESPONSES_NAMESPACES: &[&str] = &[
        "api_tool",
        "browser",
        "computer",
        "container",
        "file_search",
        "functions",
        "image_gen",
        "multi_tool_use",
        "python",
        "python_user_visible",
        "submodel_delegator",
        "terminal",
        "tool_search",
        "web",
    ];

    let Some(namespace) = namespace else {
        return Ok(());
    };
    if namespace.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must not be empty"),
        ));
    }
    if namespace.trim() != namespace {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must not have leading or trailing whitespace"),
        ));
    }
    if !namespace
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must match ^[a-zA-Z0-9_-]+$"),
        ));
    }
    if namespace.chars().count() > MAX_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} must be at most {MAX_LEN} characters"),
        ));
    }
    if namespace == "mcp"
        || namespace.starts_with("mcp__")
        || RESERVED_RESPONSES_NAMESPACES.contains(&namespace)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{LABEL} uses a reserved namespace: {namespace}"),
        ));
    }

    Ok(())
}

impl Config {
    pub(crate) async fn load_config_with_layer_stack(
        fs: &dyn ExecutorFileSystem,
        cfg: ConfigToml,
        overrides: ConfigOverrides,
        codex_home: AbsolutePathBuf,
        config_layer_stack: ConfigLayerStack,
    ) -> std::io::Result<Self> {
        // Keep the large config-construction future off small test thread stacks.
        Box::pin(async move {
        if cfg.experimental_thread_store_endpoint.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "`experimental_thread_store_endpoint` is no longer supported; remove it from config.toml",
            ));
        }

        validate_model_providers(&cfg.model_providers)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        if cfg.model_post_turn_compact_threshold_percent.is_some_and(|percent| percent > 100) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "model_post_turn_compact_threshold_percent must be between 0 and 100",
            ));
        }
        if let Some(responses_api_metadata) = cfg.responses_api_metadata.as_ref() {
            validate_extra_metadata(responses_api_metadata.iter()).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
        }
        let mut startup_warnings = config_layer_stack
            .startup_warnings()
            .unwrap_or_default()
            .to_vec();
        // Destructure ConfigOverrides fully to ensure all overrides are applied.
        let ConfigOverrides {
            model,
            review_model: override_review_model,
            cwd,
            model_provider,
            service_tier: service_tier_override,
            codex_self_exe,
            base_instructions,
            developer_instructions,
            personality,
            compact_prompt,
            show_raw_agent_reasoning,
            tools_web_search_request: override_tools_web_search_request,
            ephemeral,
            workspace_roots: workspace_roots_override,
        } = overrides;

        if let Some(profile) = cfg.profile.as_deref() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "legacy `profile = \"{profile}\"` config is no longer supported; use `--profile {profile}` with `{profile}.config.toml` instead"
                ),
            ));
        }

        let feature_overrides = FeatureOverrides {
            web_search_request: override_tools_web_search_request,
        };

        let features = Features::from_sources(
            FeatureConfigSource {
                features: cfg.features.as_ref(),
            },
            FeatureConfigSource {
                ..Default::default()
            },
            feature_overrides,
        );
        let mcp_enterprise_managed_auth = McpEnterpriseManagedAuthConfig::resolve(
            &config_layer_stack,
            cfg.mcp_enterprise_managed_auth.as_ref(),
        )?;
        let non_prefixed_mcp_tool_servers = if features.enabled(Feature::NonPrefixedMcpToolNames) {
            cfg.features
                .as_ref()
                .and_then(|features| features.non_prefixed_mcp_tool_names.as_ref())
                .and_then(|feature| match feature {
                    FeatureToml::Enabled(_) => None,
                    FeatureToml::Config(config) => config.server_names.clone(),
                })
        } else {
            None
        };
        let respect_system_proxy = features.enabled(Feature::RespectSystemProxy);
        let resolved_cwd = AbsolutePathBuf::try_from(normalize_for_native_workdir({
            use std::env;

            match cwd {
                None => {
                    tracing::info!("cwd not set, using current dir");
                    env::current_dir()?
                }
                Some(p) if p.is_absolute() => p,
                Some(p) => {
                    // Resolve relative path against the current working directory.
                    tracing::info!("cwd is relative, resolving against current dir");
                    let mut current = env::current_dir()?;
                    current.push(p);
                    current
                }
            }
        }))?;
        let memories_config: MemoriesConfig = cfg.memories.clone().unwrap_or_default().into();

        let workspace_roots_explicit = workspace_roots_override.is_some();
        let mut workspace_roots =
            workspace_roots_override.unwrap_or_else(|| vec![resolved_cwd.clone()]);
        dedupe_absolute_paths(&mut workspace_roots);
        let web_search_mode =
            resolve_web_search_mode(&cfg, &features).unwrap_or(WebSearchMode::Cached);
        let web_search_config = resolve_web_search_config(&cfg);
        let experimental_request_user_input_enabled =
            resolve_experimental_request_user_input_enabled(&cfg);
        let update_plan_enabled = resolve_update_plan_enabled(&cfg);
        let tool_registry = ToolRegistryConfig {
            error_on_tool_collisions: cfg
                .features
                .as_ref()
                .and_then(|features| features.tool_registry.as_ref())
                .and_then(|config| config.error_on_tool_collisions)
                .unwrap_or_default(),
            turn_metadata_includes_tool_info: cfg
                .features
                .as_ref()
                .and_then(|features| features.tool_registry.as_ref())
                .and_then(|config| config.turn_metadata_includes_tool_info)
                .unwrap_or_default(),
        };
        let multi_agent_v2 = resolve_multi_agent_v2_config(&cfg);
        let token_budget = resolve_token_budget_config(&cfg, &features)?;
        let rollout_budget = resolve_rollout_budget_config(&cfg, &features)?;
        let current_time_reminder = resolve_current_time_reminder_config(&cfg, &features)?;
        let sleep_tool_mode = cfg
            .features
            .as_ref()
            .and_then(|features| features.sleep_tool.as_ref())
            .and_then(|feature| match feature {
                FeatureToml::Enabled(_) => None,
                FeatureToml::Config(config) => config.mode,
            })
            .unwrap_or_default();

        let agent_roles =
            load_agent_roles(fs, &cfg, &config_layer_stack, &mut startup_warnings).await?;

        let openai_base_url = cfg
            .openai_base_url
            .clone()
            .filter(|value| !value.is_empty());

        let model_providers =
            merge_configured_model_providers(built_in_model_providers(openai_base_url), cfg.model_providers)
                .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))?;

        let model_provider_id = model_provider
            .or(cfg.model_provider)
            .unwrap_or_else(|| "openai".to_string());
        let model_provider = model_providers
            .get(&model_provider_id)
            .ok_or_else(|| {
                let message = if model_provider_id == LEGACY_OLLAMA_CHAT_PROVIDER_ID {
                    OLLAMA_CHAT_PROVIDER_REMOVED_ERROR.to_string()
                } else {
                    format!("Model provider `{model_provider_id}` not found")
                };
                std::io::Error::new(std::io::ErrorKind::NotFound, message)
            })?
            .clone();

        let shell_environment_policy = ShellEnvironmentPolicy::from(cfg.shell_environment_policy);
        let allow_login_shell = cfg.allow_login_shell.unwrap_or(true);

        let history = cfg.history.unwrap_or_default();

        if multi_agent_v2.max_concurrent_threads_per_session == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.max_concurrent_threads_per_session must be at least 1",
            ));
        }
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.min_wait_timeout_ms",
            multi_agent_v2.min_wait_timeout_ms,
        )?;
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.max_wait_timeout_ms",
            multi_agent_v2.max_wait_timeout_ms,
        )?;
        validate_multi_agent_v2_wait_timeout(
            "features.multi_agent_v2.default_wait_timeout_ms",
            multi_agent_v2.default_wait_timeout_ms,
        )?;
        if multi_agent_v2.min_wait_timeout_ms > multi_agent_v2.max_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.min_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms",
            ));
        }
        if multi_agent_v2.default_wait_timeout_ms < multi_agent_v2.min_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.default_wait_timeout_ms must be at least features.multi_agent_v2.min_wait_timeout_ms",
            ));
        }
        if multi_agent_v2.default_wait_timeout_ms > multi_agent_v2.max_wait_timeout_ms {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "features.multi_agent_v2.default_wait_timeout_ms must be at most features.multi_agent_v2.max_wait_timeout_ms",
            ));
        }
        validate_multi_agent_v2_tool_namespace(multi_agent_v2.tool_namespace.as_deref())?;
        let agents_enabled = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.enabled)
            .unwrap_or(true);
        let agent_max_threads = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.max_concurrent_threads_per_session);
        if agent_max_threads == Some(0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "agents.max_concurrent_threads_per_session must be at least 1",
            ));
        }
        let agent_max_depth = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.max_depth)
            .unwrap_or(DEFAULT_AGENT_MAX_DEPTH);
        let agent_default_subagent_model = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.default_subagent_model.clone());
        let agent_default_subagent_reasoning_effort = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.default_subagent_reasoning_effort.clone());
        let agent_interrupt_message_enabled = cfg
            .agents
            .as_ref()
            .and_then(|agents| agents.interrupt_message)
            .unwrap_or(true);
        let background_terminal_max_timeout = cfg
            .background_terminal_max_timeout
            .unwrap_or(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
            .max(MIN_EMPTY_YIELD_TIME_MS);
        let thread_unload_delay =
            Duration::from_secs(cfg.thread_unload_delay_secs.unwrap_or(/*default*/ 1800));
        if std::time::Instant::now()
            .checked_add(thread_unload_delay)
            .is_none()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "thread_unload_delay_secs is too large",
            ));
        }

        let ghost_snapshot = {
            let mut config = GhostSnapshotConfig::default();
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(ignore_over_bytes) = ghost_snapshot.ignore_large_untracked_files
            {
                config.ignore_large_untracked_files = if ignore_over_bytes > 0 {
                    Some(ignore_over_bytes)
                } else {
                    None
                };
            }
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(threshold) = ghost_snapshot.ignore_large_untracked_dirs
            {
                config.ignore_large_untracked_dirs =
                    if threshold > 0 { Some(threshold) } else { None };
            }
            if let Some(ghost_snapshot) = cfg.ghost_snapshot.as_ref()
                && let Some(disable_warnings) = ghost_snapshot.disable_warnings
            {
                config.disable_warnings = disable_warnings;
            }
            config
        };

        let forced_chatgpt_workspace_id = cfg
            .forced_chatgpt_workspace_id
            .clone()
            .map(codex_config::config_toml::ForcedChatgptWorkspaceIds::into_vec)
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|values| !values.is_empty());

        let forced_login_method = cfg.forced_login_method;

        let model = model.or(cfg.model);
        let service_tier = match service_tier_override {
            Some(Some(service_tier)) => Some(service_tier),
            Some(None) => Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()),
            None => cfg.service_tier,
        };
        let service_tier = service_tier.and_then(|service_tier| {
            match ServiceTier::from_request_value(&service_tier) {
                Some(ServiceTier::Fast) => features
                    .enabled(Feature::FastMode)
                    .then(|| ServiceTier::Fast.request_value().to_string()),
                Some(ServiceTier::Flex) => Some(ServiceTier::Flex.request_value().to_string()),
                None if service_tier == "ultrafast" => features
                    .enabled(Feature::UltrafastMode)
                    .then_some(service_tier),
                None => Some(service_tier),
            }
        });

        let compact_prompt = compact_prompt.or(cfg.compact_prompt).and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        // Load base instructions override from a file if specified. If the
        // path is relative, resolve it against the effective cwd so the
        // behaviour matches other path-like config values.
        let model_instructions_path = cfg.model_instructions_file.as_ref();
        let file_base_instructions = Self::try_read_non_empty_file(
            fs,
            model_instructions_path,
            "model instructions file",
        )
        .await?;
        let base_instructions = base_instructions
            .or(file_base_instructions)
            .or(cfg.instructions.clone());
        let base_instructions_provenance = base_instructions
            .as_ref()
            .map(|_| BaseInstructionsProvenance::Custom);
        let developer_instructions = developer_instructions.or(cfg.developer_instructions);
        let include_collaboration_mode_instructions =
            cfg.include_collaboration_mode_instructions.unwrap_or(true);
        let include_skill_instructions = cfg
            .skills
            .as_ref()
            .and_then(|skills| skills.include_instructions)
            .unwrap_or(true);
        let skill_max_context_tokens = cfg
            .skills
            .as_ref()
            .and_then(|skills| skills.max_context_tokens);
        let include_environment_context = cfg.include_environment_context.unwrap_or(true);
        let include_environment_context_time = cfg.include_environment_context_time.unwrap_or(true);
        let personality = personality.or(cfg.personality);

        let experimental_compact_prompt_path = cfg.experimental_compact_prompt_file.as_ref();
        let file_compact_prompt = Self::try_read_non_empty_file(
            fs,
            experimental_compact_prompt_path,
            "experimental compact prompt file",
        )
        .await?;
        let compact_prompt = compact_prompt.or(file_compact_prompt);
        let review_model = override_review_model.or(cfg.review_model);

        let model_catalog = load_model_catalog(cfg.model_catalog_json.clone())?;

        let log_dir = cfg
            .log_dir
            .as_ref()
            .map(AbsolutePathBuf::to_path_buf)
            .unwrap_or_else(|| codex_home.join("log").to_path_buf());
        let sqlite_home_env = resolve_sqlite_home_env(&resolved_cwd);
        let sqlite_home = cfg
            .sqlite_home
            .as_ref()
            .cloned()
            .or(sqlite_home_env)
            .unwrap_or_else(|| codex_home.clone());
        let otel = otel::resolve_config(cfg.otel.unwrap_or_default(), &mut startup_warnings);
        let config = Self {
            model,
            service_tier,
            review_model,
            model_context_window: cfg.model_context_window,
            model_auto_compact_token_limit: cfg.model_auto_compact_token_limit,
            model_auto_compact_token_limit_scope: cfg
                .model_auto_compact_token_limit_scope
                .unwrap_or_default(),
            model_post_turn_compact_threshold_percent: cfg
                .model_post_turn_compact_threshold_percent
                .unwrap_or_default(),
            model_provider_id,
            model_provider,
            cwd: resolved_cwd,
            workspace_roots: workspace_roots.clone(),
            workspace_roots_explicit,
            startup_warnings,
            allow_login_shell,
            shell_environment_policy,
            notify: cfg.notify,
            base_instructions,
            base_instructions_provenance,
            personality,
            developer_instructions,
            compact_prompt,
            include_collaboration_mode_instructions,
            include_skill_instructions,
            skill_max_context_tokens,
            include_environment_context,
            include_environment_context_time,
            // The config.toml omits "_mode" because it's a config file. However, "_mode"
            // is important in code to differentiate the mode from the store implementation.
            cli_auth_credentials_store_mode: resolve_cli_auth_credentials_store_mode(
                cfg.cli_auth_credentials_store.unwrap_or_default(),
                env!("CARGO_PKG_VERSION"),
            ),
            mcp_servers: cfg.mcp_servers,
            non_prefixed_mcp_tool_servers,
            mcp_enterprise_managed_auth,
            // The config.toml omits "_mode" because it's a config file. However, "_mode"
            // is important in code to differentiate the mode from the store implementation.
            mcp_oauth_credentials_store_mode: resolve_mcp_oauth_credentials_store_mode(
                cfg.mcp_oauth_credentials_store.unwrap_or_default(),
                env!("CARGO_PKG_VERSION"),
            ),
            mcp_oauth_callback_port: cfg.mcp_oauth_callback_port,
            mcp_oauth_callback_url: cfg.mcp_oauth_callback_url.clone(),
            mcp_optional_startup_grace: cfg
                .mcp_optional_startup_grace_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_OPTIONAL_MCP_STARTUP_GRACE),
            model_providers,
            project_doc_max_bytes: cfg.project_doc_max_bytes.unwrap_or(AGENTS_MD_MAX_BYTES),
            project_doc_fallback_filenames: cfg
                .project_doc_fallback_filenames
                .unwrap_or_default()
                .into_iter()
                .filter_map(|name| {
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .collect(),
            tool_output_token_limit: cfg.tool_output_token_limit,
            agents_enabled,
            agent_max_threads,
            agent_default_subagent_model,
            agent_default_subagent_reasoning_effort,
            agent_max_depth,
            agent_roles,
            max_goal_token_budget: cfg
                .goals
                .as_ref()
                .and_then(|goals| goals.max_goal_token_budget)
                .map(|max_goal_token_budget| {
                    i64::try_from(max_goal_token_budget.get()).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "goals.max_goal_token_budget exceeds the maximum supported token budget",
                        )
                    })
                })
                .transpose()?,
            memories: memories_config,
            agent_interrupt_message_enabled,
            codex_home,
            sqlite: codex_state::SqliteConfig::from_sqlite_home(sqlite_home),
            log_dir,
            config_layer_stack,
            application_auth_route_config: None,
            history,
            ephemeral: ephemeral.unwrap_or_default(),
            extra_config: None,
            codex_self_exe,

            hide_agent_reasoning: cfg.hide_agent_reasoning.unwrap_or(false),
            show_raw_agent_reasoning: cfg
                .show_raw_agent_reasoning
                .or(show_raw_agent_reasoning)
                .unwrap_or(false),
            model_reasoning_effort: cfg.model_reasoning_effort,
            plan_mode_reasoning_effort: cfg.plan_mode_reasoning_effort,
            model_reasoning_summary: cfg.model_reasoning_summary,
            model_catalog,
            model_verbosity: cfg.model_verbosity,
            chatgpt_base_url: cfg
                .chatgpt_base_url
                .unwrap_or("https://chatgpt.com/backend-api/".to_string()),
            respect_system_proxy,
            responses_api_metadata: cfg.responses_api_metadata.unwrap_or_default(),
            experimental_thread_store: thread_store_config(cfg.experimental_thread_store),
            forced_chatgpt_workspace_id,
            forced_login_method,
            web_search_mode,
            web_search_config,
            experimental_request_user_input_enabled,
            update_plan_enabled,
            tool_registry,
            background_terminal_max_timeout,
            thread_unload_delay,
            ghost_snapshot,
            multi_agent_v2,
            token_budget,
            token_budget_startup_config: None,
            rollout_budget,
            current_time_reminder,
            sleep_tool_mode,
            features,
            runtime_feature_defaults: BTreeMap::new(),
            suppress_unstable_features_warning: cfg
                .suppress_unstable_features_warning
                .unwrap_or(false),
            analytics_enabled: cfg.analytics.as_ref().and_then(|a| a.enabled),
            otel,
        };
        Ok(config)
        })
        .await
    }

    /// If `path` is `Some`, attempts to read the file at the given path and
    /// returns its contents as a trimmed `String`. If the file is empty, or
    /// is `Some` but cannot be read, returns an `Err`.
    async fn try_read_non_empty_file(
        fs: &dyn ExecutorFileSystem,
        path: Option<&AbsolutePathBuf>,
        context: &str,
    ) -> std::io::Result<Option<String>> {
        let Some(path) = path else {
            return Ok(None);
        };

        let path_uri = PathUri::from_abs_path(path);
        let contents = fs
            .read_file_text(&path_uri, ReadFileOptions::default())
            .await
            .map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!("failed to read {context} {}: {e}", path.display()),
                )
            })?;

        let s = contents.trim().to_string();
        if s.is_empty() {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{context} is empty: {}", path.display()),
            ))
        } else {
            Ok(Some(s))
        }
    }

    pub fn bundled_skills_enabled(&self) -> bool {
        codex_config::bundled_skills_enabled_from_stack(&self.config_layer_stack)
    }
}

/// Returns the path to the Codex configuration directory, which can be
/// specified by the `CODEX_HOME` environment variable. If not set, defaults to
/// `~/.codex`.
///
/// - If `CODEX_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `CODEX_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    codex_utils_home_dir::find_codex_home()
}

/// Returns the path to the folder where Codex logs are stored. Does not verify
/// that the directory exists.
pub fn log_dir(cfg: &Config) -> std::io::Result<PathBuf> {
    Ok(cfg.log_dir.clone())
}

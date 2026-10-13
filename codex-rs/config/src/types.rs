//! Types used to define loaded and effective Codex configuration values.

// Note this file should generally be restricted to simple struct/enum
// definitions that do not contain business logic.

pub use crate::mcp_ema::McpEmaAuthScope;
pub use crate::mcp_ema::McpEnterpriseManagedAuthConfig;
pub use crate::mcp_ema::McpServerIdpOAuthConfig;
pub use crate::mcp_types::McpServerAuth;
pub use crate::mcp_types::McpServerConfig;
pub use crate::mcp_types::McpServerDisabledReason;
pub use crate::mcp_types::McpServerEnvVar;
pub use crate::mcp_types::McpServerOAuthConfig;
pub use crate::mcp_types::McpServerToolConfig;
pub use crate::mcp_types::McpServerTransportConfig;
pub use crate::mcp_types::McpStartupReadiness;
pub use crate::mcp_types::RawMcpServerConfig;
pub use crate::shell_environment_policy::ShellEnvironmentPolicyToml;
pub use codex_protocol::config_types::ModeKind;
pub use codex_protocol::config_types::Personality;
pub use codex_protocol::config_types::ServiceTier;
pub use codex_protocol::config_types::WebSearchMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

pub const DEFAULT_OTEL_ENVIRONMENT: &str = "dev";

/// Determine where Codex should store CLI auth credentials.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthCredentialsStoreMode {
    #[default]
    /// Persist credentials in CODEX_HOME/auth.json.
    File,
    /// Persist credentials in the keyring. Fail if unavailable.
    Keyring,
    /// Use keyring when available; otherwise, fall back to a file in CODEX_HOME.
    Auto,
    /// Store credentials in memory only for the current process.
    Ephemeral,
}

/// Determine where Codex should store and read MCP credentials.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OAuthCredentialsStoreMode {
    /// Prefer `Keyring` and use `File` when keyring storage is unavailable.
    /// Once an MCP client loads credentials from one store, that client keeps the resolved store
    /// for its lifetime so refreshes cannot switch to a possibly stale credential source.
    /// Credentials stored in the keyring will only be readable by Codex unless the user explicitly grants access via OS-level keyring access.
    #[default]
    Auto,
    /// CODEX_HOME/.credentials.json
    /// This file will be readable to Codex and other applications running as the same user.
    File,
    /// Keyring when available, otherwise fail.
    Keyring,
}

/// Determine how auth credentials should use keyring-backed storage.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthKeyringBackendKind {
    /// Store the serialized auth payload directly in the OS keyring.
    Direct,
    /// Store auth payloads in the local encrypted secrets file, with the file key in the OS keyring.
    Secrets,
}

impl Default for AuthKeyringBackendKind {
    fn default() -> Self {
        if cfg!(windows) {
            Self::Secrets
        } else {
            Self::Direct
        }
    }
}

// ===== Analytics configuration =====

/// Analytics settings loaded from config.toml. Fields are optional so we can apply defaults.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AnalyticsConfigToml {
    /// When `false`, disables OpenAI analytics across Codex product surfaces in this profile.
    /// Custom OTLP metrics exporters remain enabled; set `otel.metrics_exporter = "none"` to disable them.
    pub enabled: Option<bool>,
}

// ===== OTEL configuration =====

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OtelHttpProtocol {
    /// Binary payload
    Binary,
    /// JSON payload
    Json,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct OtelTlsConfig {
    pub ca_certificate: Option<AbsolutePathBuf>,
    pub client_certificate: Option<AbsolutePathBuf>,
    pub client_private_key: Option<AbsolutePathBuf>,
}

/// Which OTEL exporter to use.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub enum OtelExporterKind {
    None,
    Statsig,
    OtlpHttp {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        protocol: OtelHttpProtocol,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
    OtlpGrpc {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
}

/// OTEL settings loaded from config.toml. Fields are optional so we can apply defaults.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OtelConfigToml {
    /// Byte limit for tool-result log output; independent of model-visible output.
    #[serde(default)]
    pub tool_result: codex_protocol::config_types::ToolResultLogConfig,
    /// Log user prompt in traces
    pub log_user_prompt: Option<bool>,
    /// Opt in to logging final main-agent and spawned-subagent responses to an OTLP log exporter.
    /// Defaults to false. Response text can be sensitive and is capped at 64 KiB.
    pub log_agent_responses: Option<bool>,

    /// Mark traces with environment (dev, staging, prod, test). Defaults to dev.
    pub environment: Option<String>,

    /// Optional log exporter
    pub exporter: Option<OtelExporterKind>,

    /// Optional trace exporter
    pub trace_exporter: Option<OtelExporterKind>,

    /// Metrics exporter. Defaults to `statsig`, which follows `analytics.enabled`.
    /// Custom OTLP exporters are independent of `analytics.enabled`; `none` disables metrics export.
    pub metrics_exporter: Option<OtelExporterKind>,

    /// Attributes to add to every exported trace span.
    pub span_attributes: Option<BTreeMap<String, String>>,

    /// Semicolon-separated `key:value` fields to upsert into W3C tracestate members.
    pub tracestate: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

/// Effective OTEL settings after defaults are applied.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelConfig {
    pub tool_result: codex_protocol::config_types::ToolResultLogConfig,
    pub log_user_prompt: bool,
    pub log_agent_responses: bool,
    pub environment: String,
    pub exporter: OtelExporterKind,
    pub trace_exporter: OtelExporterKind,
    pub metrics_exporter: OtelExporterKind,
    pub span_attributes: BTreeMap<String, String>,
    pub tracestate: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for OtelConfig {
    fn default() -> Self {
        OtelConfig {
            tool_result: Default::default(),
            log_user_prompt: false,
            log_agent_responses: false,
            environment: DEFAULT_OTEL_ENVIRONMENT.to_owned(),
            exporter: OtelExporterKind::None,
            trace_exporter: OtelExporterKind::None,
            metrics_exporter: OtelExporterKind::Statsig,
            span_attributes: BTreeMap::new(),
            tracestate: BTreeMap::new(),
        }
    }
}

impl OtelConfig {
    /// Response text requires a separate opt-in and an explicit OTLP log destination.
    pub fn agent_response_logging_enabled(&self) -> bool {
        self.log_agent_responses
            && matches!(
                self.exporter,
                OtelExporterKind::OtlpHttp { .. } | OtelExporterKind::OtlpGrpc { .. }
            )
    }
}

pub use crate::skills_config::BundledSkillsConfig;
pub use crate::skills_config::SkillConfig;
pub use crate::skills_config::SkillsConfig;

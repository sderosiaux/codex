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

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

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

pub use crate::skills_config::BundledSkillsConfig;
pub use crate::skills_config::SkillConfig;
pub use crate::skills_config::SkillsConfig;

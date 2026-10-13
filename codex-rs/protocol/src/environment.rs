//! Environment selections and the configuration attached to them.

use crate::config_types::ShellEnvironmentPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

/// An environment selected for a thread.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvironmentSelection {
    pub environment_id: String,
    pub cwd: PathUri,
    pub workspace_roots: Vec<PathUri>,
    pub config: EnvironmentConfigState,
}

impl TurnEnvironmentSelection {
    /// Whether these selections refer to the same environment and workspace.
    /// Unlike full selection equality, this ignores configuration: it can differ between
    /// threads that follow the same owner's configuration.
    pub fn has_same_workspace(&self, other: &Self) -> bool {
        self.environment_id == other.environment_id
            && self.cwd == other.cwd
            && self.workspace_roots == other.workspace_roots
    }
}

/// The environments captured by a thread and its fallback working directory.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvironmentSelections {
    pub legacy_fallback_cwd: AbsolutePathBuf,
    pub environments: Vec<TurnEnvironmentSelection>,
}

impl TurnEnvironmentSelections {
    pub fn new(
        legacy_fallback_cwd: AbsolutePathBuf,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> Self {
        Self {
            legacy_fallback_cwd,
            environments,
        }
    }
}

/// Configuration supplied for a thread's selected environment.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentConfigState {
    /// Preserve the existing thread-derived environment configuration.
    FromThread,
    /// The owner supplied configuration for this environment attachment.
    Ready(EnvironmentConfig),
}

/// Resolved configuration for a thread/environment attachment.
#[derive(Clone, PartialEq)]
pub struct EnvironmentConfig {
    /// Whether shell tools may start login shells in this environment.
    pub allow_login_shell: bool,
    /// Effective workspace roots resolved for this environment attachment.
    pub workspace_roots: Vec<PathUri>,
    /// Controls which environment variables shell commands may inherit.
    pub shell_environment_policy: ShellEnvironmentPolicy,
}

impl std::fmt::Debug for EnvironmentConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvironmentConfig")
            .field("allow_login_shell", &self.allow_login_shell)
            .field("workspace_roots", &self.workspace_roots)
            .field("shell_environment_policy", &"<redacted>")
            .finish()
    }
}

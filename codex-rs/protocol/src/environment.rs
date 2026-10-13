//! Environment requests, thread selections, and attachment authority.
//! Selections bind requests to the thread's capability roots before runtime use.

use crate::capabilities::EnvironmentCapabilityRoots;
use crate::capabilities::SelectedCapabilityRoot;
use crate::config_types::ShellEnvironmentPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;

/// An environment requested by a caller, before attaching the receiving thread's roots.
///
/// Callers choose the environment and paths, but startup may still need to load roots from
/// saved history or inherited attachments. Keeping this input separate lets the receiving
/// thread construct a complete selection instead of filling in missing roots later.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvironmentRequest {
    pub environment_id: String,
    pub cwd: PathUri,
    pub workspace_roots: Vec<PathUri>,
    pub config: EnvironmentConfigState,
}

/// An environment selected for a thread, with its capability roots already attached.
///
/// Runtime snapshots carry this value so capability discovery uses the roots captured
/// with the environment. Construct it from a request and the receiving thread's roots.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvironmentSelection {
    pub environment_id: String,
    pub cwd: PathUri,
    pub workspace_roots: Vec<PathUri>,
    pub config: EnvironmentConfigState,
    /// Roots selected by the client for this environment when the thread started.
    pub selected_capability_roots: EnvironmentCapabilityRoots,
}

impl TurnEnvironmentSelection {
    /// Attaches this environment's roots before the selection enters runtime state.
    /// The caller supplies the receiving thread's roots so reselection restores them and
    /// reusing an environment from another thread does not reuse that thread's root choices.
    pub fn new(request: TurnEnvironmentRequest, roots: &[SelectedCapabilityRoot]) -> Self {
        Self {
            selected_capability_roots: EnvironmentCapabilityRoots::for_environment(
                &request.environment_id,
                roots,
            ),
            environment_id: request.environment_id,
            cwd: request.cwd,
            workspace_roots: request.workspace_roots,
            config: request.config,
        }
    }

    /// Whether these selections refer to the same environment and workspace.
    /// Unlike full selection equality, this ignores configuration and capability roots:
    /// those can differ between threads that follow the same owner's configuration.
    pub fn has_same_workspace(&self, other: &Self) -> bool {
        self.environment_id == other.environment_id
            && self.cwd == other.cwd
            && self.workspace_roots == other.workspace_roots
    }

    /// Requests this environment again; the receiving thread supplies its own roots.
    /// This explicit conversion discards thread-owned state. Test inputs should instead
    /// construct requests directly so they do not depend on runtime selection fields.
    pub fn into_request(self) -> TurnEnvironmentRequest {
        TurnEnvironmentRequest {
            environment_id: self.environment_id,
            cwd: self.cwd,
            workspace_roots: self.workspace_roots,
            config: self.config,
        }
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

    /// Requests these environments again, leaving root choices to the receiving thread.
    pub fn into_requests(self) -> TurnEnvironmentRequests {
        TurnEnvironmentRequests {
            legacy_fallback_cwd: self.legacy_fallback_cwd,
            environment_requests: self
                .environments
                .into_iter()
                .map(TurnEnvironmentSelection::into_request)
                .collect(),
        }
    }
}

/// Environment input supplied together with its fallback working directory.
/// The receiving thread attaches roots in `select` before capturing these environments.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnvironmentRequests {
    pub legacy_fallback_cwd: AbsolutePathBuf,
    pub environment_requests: Vec<TurnEnvironmentRequest>,
}

impl TurnEnvironmentRequests {
    pub fn new(
        legacy_fallback_cwd: AbsolutePathBuf,
        environment_requests: Vec<TurnEnvironmentRequest>,
    ) -> Self {
        Self {
            legacy_fallback_cwd,
            environment_requests,
        }
    }

    /// Constructs selections with the receiving thread's roots, including roots retained
    /// while an environment was deselected. Snapshots can then carry complete selections.
    pub fn select(self, roots: &[SelectedCapabilityRoot]) -> TurnEnvironmentSelections {
        TurnEnvironmentSelections {
            legacy_fallback_cwd: self.legacy_fallback_cwd,
            environments: self
                .environment_requests
                .into_iter()
                .map(|request| TurnEnvironmentSelection::new(request, roots))
                .collect(),
        }
    }
}

/// Configuration supplied for a thread's selected environment.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum EnvironmentConfigState {
    /// Preserve the existing thread-derived environment configuration.
    FromThread,
    /// The owner will supply environment configuration later.
    Pending,
    /// The owner supplied configuration for this environment attachment.
    Ready(EnvironmentConfig),
    /// The owner could not supply configuration for this environment attachment.
    Failed(String),
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
    /// Capability roots selected for this thread's environment attachment.
    pub selected_capability_roots: Vec<SelectedCapabilityRoot>,
}

impl std::fmt::Debug for EnvironmentConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnvironmentConfig")
            .field("allow_login_shell", &self.allow_login_shell)
            .field("workspace_roots", &self.workspace_roots)
            .field("shell_environment_policy", &"<redacted>")
            .field("selected_capability_roots", &self.selected_capability_roots)
            .finish()
    }
}

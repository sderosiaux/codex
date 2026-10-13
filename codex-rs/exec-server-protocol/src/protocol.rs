use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
pub use codex_file_system::WalkOptions;
pub use codex_file_system::WalkOutcome;
use codex_install_context::InstallContext;
use codex_protocol::ThreadId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_shell_command::shell_detect::DetectedShell;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

use crate::ProcessId;

pub const INITIALIZE_METHOD: &str = "initialize";
pub const INITIALIZED_METHOD: &str = "initialized";
pub const EXEC_METHOD: &str = "process/start";
pub const EXEC_READ_METHOD: &str = "process/read";
pub const EXEC_WRITE_METHOD: &str = "process/write";
pub const EXEC_SIGNAL_METHOD: &str = "process/signal";
pub const EXEC_TERMINATE_METHOD: &str = "process/terminate";
pub const EXEC_OUTPUT_DELTA_METHOD: &str = "process/output";
pub const EXEC_EXITED_METHOD: &str = "process/exited";
pub const EXEC_CLOSED_METHOD: &str = "process/closed";
pub const ENVIRONMENT_INFO_METHOD: &str = "environment/info";
pub const ENVIRONMENT_STATUS_METHOD: &str = "environment/status";
pub const FS_READ_FILE_METHOD: &str = "fs/readFile";
pub const FS_OPEN_METHOD: &str = "fs/open";
pub const FS_READ_BLOCK_METHOD: &str = "fs/readBlock";
pub const FS_WRITE_BLOCK_METHOD: &str = "fs/writeBlock";
pub const FS_CLOSE_METHOD: &str = "fs/close";
pub const FS_WRITE_FILE_METHOD: &str = "fs/writeFile";
pub const FS_CREATE_DIRECTORY_METHOD: &str = "fs/createDirectory";
pub const FS_GET_METADATA_METHOD: &str = "fs/getMetadata";
pub const FS_CANONICALIZE_METHOD: &str = "fs/canonicalize";
pub const FS_READ_DIRECTORY_METHOD: &str = "fs/readDirectory";
pub const FS_WALK_METHOD: &str = "fs/walk";
pub const FS_REMOVE_METHOD: &str = "fs/remove";
pub const FS_COPY_METHOD: &str = "fs/copy";
/// Discovers capability manifests below selected roots using executor-local filesystem access.
pub const CAPABILITY_ROOTS_DISCOVER_METHOD: &str = "capabilityRoots/discoverV1";
/// Ordered plugin manifest paths recognized beneath a plugin root.
pub const DISCOVERABLE_PLUGIN_MANIFEST_PATHS: &[&str] = &[
    ".codex-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".cursor-plugin/plugin.json",
];
/// JSON-RPC request method for executor-side HTTP requests.
pub const HTTP_REQUEST_METHOD: &str = "http/request";
/// JSON-RPC notification method for streamed executor HTTP response bodies.
pub const HTTP_REQUEST_BODY_DELTA_METHOD: &str = "http/request/bodyDelta";
/// Maximum decoded response-body bytes carried by one streamed HTTP notification.
pub const MAX_HTTP_BODY_DELTA_BYTES: usize = 1024 * 1024;

/// Shared immutable bytes, encoded as a base64 string on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByteChunk(#[serde(with = "base64_bytes")] pub Arc<Vec<u8>>);

impl ByteChunk {
    pub fn into_inner(self) -> Vec<u8> {
        Arc::unwrap_or_clone(self.0)
    }
}

impl From<Vec<u8>> for ByteChunk {
    fn from(value: Vec<u8>) -> Self {
        Self(Arc::new(value))
    }
}

impl AsRef<[u8]> for ByteChunk {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_name: String,
    #[serde(default)]
    pub resume_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub session_id: String,
    /// Executor metadata at initialization, with the same shape as `environment/info`.
    // TODO: Make this required once all supported exec-server versions return environmentInfo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_info: Option<EnvironmentInfo>,
}

/// Information about an execution/filesystem environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentInfo {
    pub shell: ShellInfo,
    /// Executor release version for version-based compatibility decisions.
    /// `0.0.0` when unknown, including responses from legacy executors.
    #[serde(default = "unknown_executor_version")]
    pub executor_version: String,
    /// Opaque executor build identity for looking up behavioral verification.
    /// Derived from the compiled commit and target for standard builds;
    /// absent for legacy or unstamped builds. This is not an artifact checksum
    /// or a security attestation, and evidence must not be shared across build variants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Working directory inherited by the exec-server process.
    #[serde(default)]
    pub cwd: Option<PathUri>,
    /// Executor user home used to expand `~` in path-bearing values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_home_dir: Option<PathUri>,
    /// Operating system reported by the executor; absent for legacy exec-servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_os: Option<String>,
    /// Executor directories to prepend to `PATH` when missing, in priority order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prepend_path_dirs: Vec<PathUri>,
    /// Executor-local default directories for resolving `:tmpdir`, when reported.
    /// On Windows, a command's `TEMP` or `TMP` overrides take precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary_directories: Option<Vec<PathUri>>,
    /// Executor-native temporary directory for child-visible sidecars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathUri>,
    /// Optional executor features that clients must gate before sending newer request fields.
    #[serde(default)]
    pub capabilities: EnvironmentCapabilities,
}

fn unknown_executor_version() -> String {
    "0.0.0".to_string()
}

/// Features supported by the selected exec-server environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentCapabilities {
    /// Whether this executor supports V2 capability discovery.
    #[serde(default)]
    pub capability_discovery_v2: bool,
    /// Whether this executor supports the `environmentConfig/read` request.
    #[serde(default)]
    pub environment_config_read: bool,
    /// Whether HTTP headers can resolve values from the executor environment.
    #[serde(default)]
    pub http_header_env_vars: bool,
    /// Whether `fs/open` supports replacement mode and `fs/writeBlock` is supported.
    #[serde(default)]
    pub file_write_streaming: bool,
    /// Whether shell state can be cached and restored entirely inside the executor.
    #[serde(default)]
    pub shell_snapshot_v2: bool,
}

/// Status returned by an initialized exec-server connection.
///
/// The response is intentionally small today. New status details can be added
/// without changing the method used by clients to verify that an initialized
/// exec-server connection is still responsive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatus {
    pub status: EnvironmentStatusKind,
}

/// High-level status reported by exec-server itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EnvironmentStatusKind {
    /// The connection is initialized and exec-server can handle requests.
    Ready,
}

impl EnvironmentInfo {
    /// Returns executor-local default directories used to resolve `:tmpdir`.
    ///
    /// This is separate from `local` so orchestrator startup can cache the
    /// directories without repeating local shell detection.
    pub fn local_temporary_directories() -> Vec<PathUri> {
        let cwd = std::env::current_dir().ok();
        Self::local_temporary_directories_with_cwd(cwd.as_deref())
    }

    fn local_temporary_directories_with_cwd(cwd: Option<&std::path::Path>) -> Vec<PathUri> {
        let temporary_directory_env_vars: &[&str] = if cfg!(windows) {
            &["TEMP", "TMP"]
        } else {
            &["TMPDIR"]
        };
        let normalize_temp_path = |path: std::ffi::OsString| {
            PathUri::from_host_native_path(&path).ok().or_else(|| {
                if cfg!(unix) {
                    PathUri::from_host_native_path(cwd.as_ref()?.join(path)).ok()
                } else {
                    None
                }
            })
        };
        let mut temporary_directories = Vec::new();
        for name in temporary_directory_env_vars {
            if let Some(path) = std::env::var_os(name)
                .filter(|path| !path.is_empty())
                .filter(|path| cfg!(unix) || std::path::Path::new(path).is_absolute())
                .and_then(&normalize_temp_path)
                && !temporary_directories.contains(&path)
            {
                temporary_directories.push(path);
            }
        }
        temporary_directories
    }

    /// Returns information about the current local exec-server process.
    pub fn local() -> Self {
        let cwd = std::env::current_dir().ok();
        let temporary_directories = Self::local_temporary_directories_with_cwd(cwd.as_deref());
        let normalize_temp_path = |path: std::ffi::OsString| {
            PathUri::from_host_native_path(&path).ok().or_else(|| {
                if cfg!(unix) {
                    PathUri::from_host_native_path(cwd.as_ref()?.join(path)).ok()
                } else {
                    None
                }
            })
        };
        let temp_dir = normalize_temp_path(std::env::temp_dir().into_os_string());

        Self {
            shell: codex_shell_command::shell_detect::default_user_shell().into(),
            executor_version: unknown_executor_version(),
            provider_id: None,
            cwd: cwd.and_then(|cwd| PathUri::from_host_native_path(cwd).ok()),
            user_home_dir: PathUri::from_host_native_path("~").ok(),
            platform_os: Some(std::env::consts::OS.to_string()),
            prepend_path_dirs: InstallContext::current()
                .package_layout
                .as_ref()
                .and_then(|layout| layout.path_dir.as_ref())
                .into_iter()
                .map(PathUri::from_abs_path)
                .collect(),
            temporary_directories: Some(temporary_directories),
            temp_dir,
            capabilities: EnvironmentCapabilities {
                capability_discovery_v2: true,
                environment_config_read: true,
                http_header_env_vars: true,
                file_write_streaming: true,
                shell_snapshot_v2: cfg!(unix),
            },
        }
    }
}

/// Shell detected for an execution/filesystem environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellInfo {
    /// Stable shell name, for example `zsh`, `bash`, `powershell`, `sh`, or `cmd`.
    pub name: String,
    /// Target-native shell executable path or command name. Fallbacks such as `cmd.exe` need not
    /// be absolute, so this is not a [`PathUri`].
    pub path: String,
}

impl From<DetectedShell> for ShellInfo {
    fn from(shell: DetectedShell) -> Self {
        Self {
            name: shell.name().to_string(),
            path: shell.shell_path.to_string_lossy().into_owned(),
        }
    }
}

/// Optional tool attribution for executor telemetry and child environments, not authorization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecParams {
    /// Client-chosen logical process handle scoped to this connection/session.
    /// This is a protocol key, not an OS pid.
    pub process_id: ProcessId,
    /// Optional attribution; older clients omit it and older executors ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ExecMetadata>,
    pub argv: Vec<String>,
    /// Working directory URI, interpreted using the exec-server host's path rules at launch time.
    pub cwd: PathUri,
    #[serde(default)]
    pub env_policy: Option<ExecEnvPolicy>,
    /// Optional request to restore executor-owned, attachment-scoped shell state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_snapshot: Option<ShellSnapshotRequest>,
    pub env: HashMap<String, String>,
    pub tty: bool,
    /// Keep non-tty stdin writable through `process/write`.
    #[serde(default)]
    pub pipe_stdin: bool,
    /// Optional process-visible argv0 override. Values are command names rather than paths, so
    /// this is not a [`PathUri`].
    pub arg0: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecEnvPolicy {
    pub inherit: ShellEnvironmentPolicyInherit,
    pub ignore_default_excludes: bool,
    pub exclude: Vec<String>,
    pub r#set: HashMap<String, String>,
    pub include_only: Vec<String>,
}

/// Identifies shell state owned by one attachment within an executor session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellSnapshotRequest {
    /// Attachment identity; executor sessions independently scope every cache.
    pub scope_id: String,
    /// Executor-native shell used to capture and restore the snapshot.
    pub shell: ShellInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResponse {
    pub process_id: ProcessId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadParams {
    pub process_id: ProcessId,
    pub after_seq: Option<u64>,
    pub max_bytes: Option<usize>,
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOutputChunk {
    pub seq: u64,
    pub stream: ExecOutputStream,
    pub chunk: ByteChunk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResponse {
    pub chunks: Vec<ProcessOutputChunk>,
    pub next_seq: u64,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub closed: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteParams {
    pub process_id: ProcessId,
    pub chunk: ByteChunk,
    pub write_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriteStatus {
    Accepted,
    UnknownProcess,
    StdinClosed,
    Starting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResponse {
    pub status: WriteStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessSignal {
    Interrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalParams {
    pub process_id: ProcessId,
    pub signal: ProcessSignal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminateParams {
    pub process_id: ProcessId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminateResponse {
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileParams {
    pub path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileResponse {
    pub data_base64: String,
}

/// Defaults preserve read-only opens for legacy callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FsOpenMode {
    /// Open an existing file for reading.
    #[default]
    Read,
    /// Open for writing, creating a missing file or truncating an existing file.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsOpenParams {
    pub handle_id: String,
    pub path: PathUri,
    #[serde(default)]
    pub mode: FsOpenMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsOpenResponse {
    pub handle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadBlockParams {
    pub handle_id: String,
    pub offset: u64,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadBlockResponse {
    pub chunk: ByteChunk,
    pub eof: bool,
}

/// Writes a nonempty block of at most [`codex_file_system::FILE_WRITE_CHUNK_SIZE`] decoded bytes at `offset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteBlockParams {
    pub handle_id: String,
    pub offset: u64,
    pub chunk: ByteChunk,
}

/// A successful response confirms that every byte in the requested block was written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteBlockResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCloseParams {
    pub handle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCloseResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteFileParams {
    pub path: PathUri,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteFileResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCreateDirectoryParams {
    pub path: PathUri,
    pub recursive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCreateDirectoryResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataParams {
    pub path: PathUri,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataResponse {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub created_at_ms: i64,
    pub modified_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeParams {
    pub path: PathUri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeResponse {
    pub path: PathUri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryParams {
    pub path: PathUri,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryResponse {
    pub entries: Vec<FsReadDirectoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWalkParams {
    pub path: PathUri,
    pub options: WalkOptions,
}

pub type FsWalkResponse = WalkOutcome;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRemoveParams {
    pub path: PathUri,
    pub recursive: Option<bool>,
    pub force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRemoveResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCopyParams {
    pub source_path: PathUri,
    pub destination_path: PathUri,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCopyResponse {}

/// Roots to inspect for plugin and skill capability manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootsDiscoverParams {
    pub roots: Vec<CapabilityRootDiscoverRequest>,
}

/// One caller-selected capability root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootDiscoverRequest {
    /// Opaque caller identity returned unchanged in the response.
    pub id: String,
    /// Absolute root URI interpreted using the exec-server host's path rules.
    pub path: PathUri,
}

/// Executor-local discovery results in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootsDiscoverResponse {
    pub roots: Vec<CapabilityRootDiscovery>,
}

/// Recognized UTF-8 capability file materialized by the exec-server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityTextFile {
    pub path: PathUri,
    pub contents: String,
}

/// Plugin files declared directly by a selected root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPluginFiles {
    pub manifest: CapabilityTextFile,
    /// File-backed MCP declarations, including the conventional `.mcp.json` fallback.
    #[serde(default)]
    pub mcp_config: Option<CapabilityTextFile>,
    /// File-backed connector declarations.
    #[serde(default)]
    pub apps_config: Option<CapabilityTextFile>,
}

/// A skill instructions file and its optional sibling metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSkillFiles {
    pub instructions: CapabilityTextFile,
    #[serde(default)]
    pub metadata: Option<CapabilityTextFile>,
}

/// Manifest bundle for one selected root.
///
/// Discovery failures are root-local so one broken package does not discard valid siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRootDiscovery {
    pub id: String,
    pub path: PathUri,
    #[serde(default)]
    pub plugin: Option<DiscoveredPluginFiles>,
    #[serde(default)]
    pub skills: Vec<DiscoveredSkillFiles>,
    /// Plugin manifests found while scanning the root, used to namespace nested skills.
    #[serde(default)]
    pub namespace_manifests: Vec<CapabilityTextFile>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Immutable results for the selected capability roots visible in one model step.
#[derive(Clone, Debug)]
pub struct ExecutorCapabilityDiscoverySnapshot {
    roots: Arc<[ExecutorCapabilityDiscoverySnapshotEntry]>,
}

#[derive(Clone, Debug)]
pub struct ExecutorCapabilityDiscoverySnapshotEntry {
    pub selected_root: SelectedCapabilityRoot,
    pub result: Result<Arc<CapabilityRootDiscovery>, String>,
}

impl ExecutorCapabilityDiscoverySnapshot {
    pub fn new(
        selected_roots: &[SelectedCapabilityRoot],
        discoveries: Vec<Result<Arc<CapabilityRootDiscovery>, String>>,
    ) -> Self {
        debug_assert_eq!(selected_roots.len(), discoveries.len());
        Self {
            roots: selected_roots
                .iter()
                .cloned()
                .zip(discoveries)
                .map(
                    |(selected_root, result)| ExecutorCapabilityDiscoverySnapshotEntry {
                        selected_root,
                        result,
                    },
                )
                .collect(),
        }
    }

    pub fn roots(&self) -> &[ExecutorCapabilityDiscoverySnapshotEntry] {
        &self.roots
    }
}

/// HTTP header represented in the executor protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpHeader {
    /// Header name as it appears on the HTTP wire.
    pub name: String,
    /// Literal header value, or prefix for an executor-local environment value.
    pub value: String,
    /// Environment variable resolved by the process that sends the HTTP request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_env_var: Option<String>,
}

/// Redirect behavior for an executor-side HTTP request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HttpRedirectPolicy {
    /// Follow redirects using the HTTP client's normal limits.
    #[default]
    Follow,
    /// Return the redirect response without following its location.
    Stop,
}

/// Executor-side HTTP request envelope.
///
/// This intentionally stays transport-shaped rather than MCP-shaped so callers
/// can use it for Streamable HTTP, OAuth discovery, and future executor-owned
/// HTTP probes without introducing one protocol method per higher-level use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestParams {
    /// HTTP method, for example `GET`, `POST`, or `DELETE`.
    pub method: String,
    /// Absolute `http://` or `https://` URL.
    pub url: String,
    /// Ordered request headers. Repeated header names are preserved.
    #[serde(default)]
    pub headers: Vec<HttpHeader>,
    /// Optional request body bytes.
    #[serde(default, rename = "bodyBase64")]
    pub body: Option<ByteChunk>,
    /// Request timeout in milliseconds.
    ///
    /// Omitted or `null` disables the timeout. A number applies that exact
    /// millisecond deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Whether the executor should follow HTTP redirects.
    #[serde(default)]
    pub redirect_policy: HttpRedirectPolicy,
    /// Caller-chosen stream id for `http/request/bodyDelta` notifications.
    ///
    /// The id must remain unique on a connection until the terminal body delta
    /// arrives, even if the caller stops reading the stream earlier. Buffered
    /// requests still send an id so callers can keep one consistent request
    /// envelope shape.
    pub request_id: String,
    /// Return after response headers and stream the response body as deltas.
    #[serde(default)]
    pub stream_response: bool,
}

/// HTTP response envelope returned from an executor `http/request` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestResponse {
    /// Numeric HTTP response status code.
    pub status: u16,
    /// Ordered response headers. Repeated header names are preserved.
    pub headers: Vec<HttpHeader>,
    /// Buffered response body bytes. Empty when `streamResponse` is true.
    #[serde(rename = "bodyBase64")]
    pub body: ByteChunk,
}

/// Ordered response-body frame for `streamResponse` HTTP requests.
///
/// Headers are returned in the `http/request` response so the caller can choose
/// a parser immediately; body bytes then arrive on this notification stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestBodyDeltaNotification {
    /// Request id from the streamed `http/request` call.
    pub request_id: String,
    /// Monotonic one-based body frame sequence number.
    pub seq: u64,
    /// Response-body bytes carried by this frame.
    #[serde(rename = "deltaBase64")]
    pub delta: ByteChunk,
    /// Marks response-body EOF. No later deltas are expected for this request.
    #[serde(default)]
    pub done: bool,
    /// Terminal stream error. Set only on the final notification.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
    Pty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecOutputDeltaNotification {
    pub process_id: ProcessId,
    pub seq: u64,
    pub stream: ExecOutputStream,
    pub chunk: ByteChunk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecExitedNotification {
    pub process_id: ProcessId,
    pub seq: u64,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecClosedNotification {
    pub process_id: ProcessId,
    pub seq: u64,
}

mod base64_bytes {
    use super::BASE64_STANDARD;
    use base64::Engine as _;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serializer;
    use std::sync::Arc;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        BASE64_STANDARD
            .decode(encoded)
            .map(Arc::new)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn discovery_v2_support_defaults_off_for_older_executors() -> serde_json::Result<()> {
        let legacy: super::EnvironmentCapabilities = serde_json::from_value(serde_json::json!({}))?;
        assert!(!legacy.capability_discovery_v2);
        let capabilities = super::EnvironmentInfo::local().capabilities;
        assert_eq!(
            serde_json::from_value::<super::EnvironmentCapabilities>(serde_json::to_value(
                &capabilities
            )?)?,
            capabilities
        );
        Ok(())
    }

    use super::EnvironmentCapabilities;
    use super::EnvironmentInfo;

    use super::FsReadFileParams;
    use super::HttpRequestParams;

    use super::ShellInfo;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;

    #[test]
    fn shared_byte_chunks_preserve_base64_and_owned_bytes() {
        let chunk = super::ByteChunk::from(vec![0, 255, 10]);
        let shared = chunk.clone();
        assert_eq!(serde_json::to_string(&shared).unwrap(), r#""AP8K""#);
        assert_eq!(
            serde_json::from_str::<super::ByteChunk>(r#""AP8K""#).unwrap(),
            chunk,
        );

        let mut owned = shared.into_inner();
        owned[0] = 1;
        assert_eq!(
            (owned, chunk.into_inner()),
            (vec![1, 255, 10], vec![0, 255, 10])
        );
    }

    #[test]
    fn environment_info_accepts_legacy_response_without_cwd() {
        let info: EnvironmentInfo = serde_json::from_value(serde_json::json!({
            "shell": { "name": "zsh", "path": "/bin/zsh" }
        }))
        .expect("legacy environment info should deserialize");

        assert_eq!(
            info,
            EnvironmentInfo {
                shell: ShellInfo {
                    name: "zsh".to_string(),
                    path: "/bin/zsh".to_string(),
                },
                executor_version: "0.0.0".to_string(),
                provider_id: None,
                cwd: None,
                user_home_dir: None,
                platform_os: None,
                prepend_path_dirs: Vec::new(),
                temporary_directories: None,
                temp_dir: None,
                capabilities: EnvironmentCapabilities::default(),
            }
        );
    }

    #[test]
    fn environment_info_preserves_executor_metadata() {
        let expected = serde_json::json!({
            "shell": { "name": "powershell", "path": "powershell.exe" },
            "executorVersion": "1.2.3-alpha.4",
            "providerId": "sha256:e0a0cebe63ab8189ffe3eed378ccf6aa89ef15bc75e39dbbf1fc55951ec6888b",
            "cwd": null,
            "userHomeDir": "file:///C:/Users/remote",
            "platformOs": "windows",
            "prependPathDirs": ["file:///C:/tools/bin", "file:///D:/tools/bin"],
            "temporaryDirectories": ["file:///C:/Temp", "file:///D:/Temp"],
            "capabilities": {
                "capabilityDiscoveryV2": false,
                "environmentConfigRead": false,
                "httpHeaderEnvVars": false,
                "fileWriteStreaming": false,
                "shellSnapshotV2": false,
            },
        });
        let info: EnvironmentInfo = serde_json::from_value(expected.clone())
            .expect("environment info with executor metadata should deserialize");

        assert_eq!(
            serde_json::to_value(info).expect("environment info should serialize"),
            expected,
        );
    }

    #[test]
    fn local_environment_info_reads_platform_temporary_directories() {
        let cwd = std::env::current_dir().expect("current directory");
        let names: &[&str] = if cfg!(windows) {
            &["TEMP", "TMP"]
        } else {
            &["TMPDIR"]
        };
        let mut expected = names
            .iter()
            .filter_map(std::env::var_os)
            .filter(|path| !path.is_empty())
            .filter(|path| cfg!(unix) || std::path::Path::new(path).is_absolute())
            .filter_map(|path| {
                PathUri::from_host_native_path(&path).ok().or_else(|| {
                    if cfg!(unix) {
                        PathUri::from_host_native_path(cwd.join(path)).ok()
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();
        expected.dedup();

        let info = EnvironmentInfo::local();
        assert_eq!(info.temporary_directories, Some(expected));
        assert_eq!(info.user_home_dir, PathUri::from_host_native_path("~").ok());
    }

    #[cfg(unix)]
    #[test]
    fn local_environment_info_resolves_relative_temporary_directory() {
        if std::env::var_os("CODEX_TEST_RELATIVE_TMPDIR").is_none() {
            let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .arg("--exact")
                .arg(
                    "protocol::tests::local_environment_info_resolves_relative_temporary_directory",
                )
                .env("CODEX_TEST_RELATIVE_TMPDIR", "1")
                .env("TMPDIR", "relative-temp")
                .status()
                .expect("run relative TMPDIR subprocess");
            assert!(status.success(), "relative TMPDIR subprocess failed");
            return;
        }

        let expected = PathUri::from_host_native_path(
            std::env::current_dir()
                .expect("current directory")
                .join("relative-temp"),
        )
        .expect("absolute temporary directory URI");
        let info = EnvironmentInfo::local();
        assert_eq!(info.temporary_directories, Some(vec![expected.clone()]));
        assert_eq!(info.temp_dir, Some(expected));
    }

    #[test]
    fn http_request_timeout_treats_omitted_and_null_as_no_timeout() {
        let omitted: HttpRequestParams = serde_json::from_value(serde_json::json!({
            "method": "GET",
            "url": "https://example.test",
            "requestId": "req-omitted-timeout",
        }))
        .expect("omitted timeout should deserialize");
        let null_timeout: HttpRequestParams = serde_json::from_value(serde_json::json!({
            "method": "GET",
            "url": "https://example.test",
            "requestId": "req-null-timeout",
            "timeoutMs": null,
        }))
        .expect("null timeout should deserialize");
        let explicit_timeout: HttpRequestParams = serde_json::from_value(serde_json::json!({
            "method": "GET",
            "url": "https://example.test",
            "requestId": "req-explicit-timeout",
            "timeoutMs": 1234,
        }))
        .expect("numeric timeout should deserialize");

        assert_eq!(
            (omitted.request_id.as_str(), omitted.timeout_ms),
            ("req-omitted-timeout", None)
        );
        assert_eq!(
            (null_timeout.request_id.as_str(), null_timeout.timeout_ms),
            ("req-null-timeout", None)
        );
        assert_eq!(
            (
                explicit_timeout.request_id.as_str(),
                explicit_timeout.timeout_ms
            ),
            ("req-explicit-timeout", Some(1234))
        );
    }

    #[test]
    fn filesystem_protocol_rejects_native_absolute_paths() {
        let native_path = std::env::current_dir()
            .expect("current directory")
            .join("native-file.txt");

        serde_json::from_value::<FsReadFileParams>(serde_json::json!({
            "path": native_path.to_string_lossy(),
        }))
        .expect_err("native absolute path should not deserialize as a URI");
    }
}

use std::collections::HashMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
use codex_shell_command::shell_detect::DetectedShell;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

use crate::ProcessId;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
    Pty,
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

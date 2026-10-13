//! Launch MCP stdio servers and return the transport rmcp should use.
//!
//! [`LocalStdioServerLauncher`] starts the configured command as a child of this
//! process and returns a [`StdioServerTransport`] rmcp can serve over.
//! Unix servers use the stdio-only descriptor policy.

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::thread::sleep;
#[cfg(unix)]
use std::thread::spawn;
use std::time::Duration;

use codex_config::types::McpServerEnvVar;
use codex_utils_pty::Command;
#[cfg(unix)]
use codex_utils_pty::DescriptorPolicy;
use codex_utils_pty::ProcessMode;
#[cfg(unix)]
use codex_utils_pty::process_group::kill_process_group;
#[cfg(unix)]
use codex_utils_pty::process_group::terminate_process_group;
use futures::FutureExt;
use futures::future::BoxFuture;
use rmcp::service::RoleClient;
use rmcp::service::RxJsonRpcMessage;
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::info;
use tracing::warn;

use crate::local_stdio_transport::LocalStdioTransport;
use crate::program_resolver;
use crate::protocol_mode::McpProtocolMode;
use crate::utils::create_env_for_mcp_server;

// General purpose public code.

/// Launches an MCP stdio server and returns the transport for rmcp.
///
/// This trait is the boundary between MCP lifecycle code and process placement.
/// `RmcpClient` owns MCP operations such as `initialize` and `tools/list`; the
/// launcher owns starting the configured command and producing an rmcp
/// [`Transport`] over the server's stdin/stdout bytes.
pub trait StdioServerLauncher: private::Sealed + Send + Sync {
    /// Start the configured stdio server and return its rmcp-facing transport.
    fn launch(
        &self,
        command: StdioServerCommand,
    ) -> BoxFuture<'static, io::Result<StdioServerTransport>>;
}

/// Command-line process shape shared by stdio server launchers.
#[derive(Clone)]
pub struct StdioServerCommand {
    program: OsString,
    args: Vec<OsString>,
    env: Option<HashMap<OsString, OsString>>,
    env_vars: Vec<McpServerEnvVar>,
    cwd: Option<String>,
    protocol_mode: McpProtocolMode,
}

/// Client-side rmcp transport for a launched MCP stdio server.
///
/// The concrete process placement stays private to this module. `RmcpClient`
/// only sees the standard rmcp transport abstraction and can pass this value
/// directly to `rmcp::service::serve_client`.
pub struct StdioServerTransport {
    inner: LocalStdioTransport,
    process: StdioServerProcessHandle,
}

impl Transport<RoleClient> for StdioServerTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item).boxed()
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.inner.receive().boxed()
    }

    async fn close(&mut self) -> std::result::Result<(), Self::Error> {
        self.process.terminate().await?;
        self.inner.close().await
    }
}

impl StdioServerTransport {
    pub(crate) fn process_handle(&self) -> StdioServerProcessHandle {
        self.process.clone()
    }
}

impl StdioServerCommand {
    /// Build the stdio process parameters before choosing where the process
    /// runs.
    pub(super) fn new(
        program: OsString,
        args: Vec<OsString>,
        env: Option<HashMap<OsString, OsString>>,
        env_vars: Vec<McpServerEnvVar>,
        cwd: Option<String>,
        protocol_mode: McpProtocolMode,
    ) -> Self {
        Self {
            program,
            args,
            env,
            env_vars,
            cwd,
            protocol_mode,
        }
    }
}

// Local public implementation.

/// Starts MCP stdio servers as local child processes.
///
/// This is the existing behavior for local MCP servers: the orchestrator
/// process spawns the configured command and rmcp talks to the child's local
/// stdin/stdout pipes directly.
#[derive(Clone)]
pub struct LocalStdioServerLauncher {
    fallback_cwd: PathBuf,
}

impl LocalStdioServerLauncher {
    /// Creates a local stdio launcher.
    ///
    /// `fallback_cwd` is used when the MCP server config omits `cwd`, so
    /// relative commands resolve from the caller's runtime working directory.
    pub fn new(fallback_cwd: PathBuf) -> Self {
        Self { fallback_cwd }
    }
}

impl StdioServerLauncher for LocalStdioServerLauncher {
    fn launch(
        &self,
        command: StdioServerCommand,
    ) -> BoxFuture<'static, io::Result<StdioServerTransport>> {
        let fallback_cwd = self.fallback_cwd.clone();
        async move {
            // Keep synchronous program resolution and process creation from blocking the
            // caller's startup deadline.
            tokio::task::spawn_blocking(move || Self::launch_server(command, fallback_cwd))
                .await
                .map_err(io::Error::other)?
        }
        .boxed()
    }
}

// Local private implementation.

#[cfg(unix)]
const PROCESS_GROUP_TERM_GRACE_PERIOD: Duration = Duration::from_secs(2);

// Keep queued stderr diagnostics before closing the reader, even when an
// escaped descendant prevents the pipe from reaching EOF.
const STDERR_READER_DRAIN_GRACE_PERIOD: Duration = Duration::from_millis(250);

#[cfg(unix)]
struct LocalProcessTerminator {
    process_group_id: u32,
}

#[cfg(windows)]
enum LocalProcessTerminator {
    Job(codex_utils_pty::JobObject),
    Process(OwnedHandle),
}

#[cfg(not(any(unix, windows)))]
struct LocalProcessTerminator;

#[derive(Clone)]
pub(crate) struct StdioServerProcessHandle {
    inner: Arc<StdioServerProcessHandleInner>,
}

struct StdioServerProcessHandleInner {
    terminator: Option<LocalProcessTerminator>,
    terminated: AtomicBool,
    // An escaped descendant can keep stderr open after the MCP server exits.
    stderr_reader: Option<watch::Sender<()>>,
}

mod private {
    pub trait Sealed {}
}

impl private::Sealed for LocalStdioServerLauncher {}

impl LocalStdioServerLauncher {
    fn launch_server(
        command: StdioServerCommand,
        fallback_cwd: PathBuf,
    ) -> io::Result<StdioServerTransport> {
        let StdioServerCommand {
            program,
            args,
            env,
            env_vars,
            cwd,
            protocol_mode,
        } = command;
        let program_name = program.to_string_lossy().into_owned();
        let envs = create_env_for_mcp_server(env, &env_vars).map_err(io::Error::other)?;
        let cwd = cwd.map(PathBuf::from).unwrap_or(fallback_cwd);
        let resolved_program =
            program_resolver::resolve(program, &envs, &cwd).map_err(io::Error::other)?;

        let build_command = || {
            let mut command = Command::new(&resolved_program);
            command.current_dir(&cwd).envs(&envs).args(&args);
            command.process_mode(ProcessMode::NewGroup);
            // MCP uses only stdio; select Explicit to exclude unrelated
            // orchestrator descriptors from the server and commands it launches.
            // Descriptor allowlisting is Unix-only. Windows can still inherit unrelated
            // handles and needs a handle allowlist in the shared spawn backend.
            #[cfg(unix)]
            command.descriptor_policy(DescriptorPolicy::Explicit);
            command
        };
        #[cfg(windows)]
        let mut command = build_command();
        #[cfg(not(windows))]
        let command = build_command();
        #[cfg(windows)]
        let job = match codex_utils_pty::JobObject::create_without_breakaway() {
            Ok(job) => {
                command.prepare_suspended_spawn(&job);
                Some(job)
            }
            Err(error) => {
                warn!("Windows MCP process job containment unavailable: {error}");
                None
            }
        };

        let spawn_transport = |command: Command| -> io::Result<(
            LocalStdioTransport,
            Option<tokio::process::ChildStderr>,
            Option<u32>,
        )> {
            let (transport, stderr) =
                LocalStdioTransport::spawn(command, program_name.clone(), protocol_mode)?;
            let process_id = transport.id();
            Ok((transport, stderr, process_id))
        };
        let (transport, stderr, process_id) = spawn_transport(command)?;
        #[cfg(windows)]
        let (transport, stderr, process_id, job) = match job {
            Some(job) => match process_id
                .ok_or_else(|| io::Error::other("missing suspended MCP server process id"))
                .and_then(|process_id| job.assign_and_resume_process(process_id))
            {
                Ok(true) => (transport, stderr, process_id, Some(job)),
                Ok(false) => (transport, stderr, process_id, None),
                Err(error) => {
                    warn!(
                        "Windows MCP process job containment failed; retrying without it: {error}"
                    );
                    drop(stderr);
                    drop(transport);
                    drop(job);
                    let (transport, stderr, process_id) = spawn_transport(build_command())?;
                    (transport, stderr, process_id, None)
                }
            },
            None => (transport, stderr, process_id, None),
        };
        #[cfg(windows)]
        let terminator = match job {
            Some(job) => Some(LocalProcessTerminator::Job(job)),
            None => process_id.and_then(|process_id| {
                match codex_utils_pty::JobObject::open_process_handle(process_id) {
                    Ok(handle) => Some(LocalProcessTerminator::Process(handle)),
                    Err(error) => {
                        warn!("Windows MCP process handle unavailable: {error}");
                        None
                    }
                }
            }),
        };
        #[cfg(not(windows))]
        let terminator = process_id.map(LocalProcessTerminator::new);
        let stderr_reader = stderr.map(|stderr| {
            let program_name = program_name.clone();
            let (stop_tx, mut stop_rx) = watch::channel(());
            std::mem::drop(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                // Give queued diagnostics time to reach the logs without waiting
                // indefinitely for a descendant that still has stderr open.
                let drain_deadline = tokio::time::sleep(STDERR_READER_DRAIN_GRACE_PERIOD);
                tokio::pin!(drain_deadline);
                let mut draining = false;
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut drain_deadline, if draining => break,
                        _ = stop_rx.changed(), if !draining => {
                            draining = true;
                            drain_deadline.as_mut().reset(
                                Instant::now() + STDERR_READER_DRAIN_GRACE_PERIOD
                            );
                        }
                        line = reader.next_line() => {
                            match line {
                                Ok(Some(line)) => {
                                    info!("MCP server stderr ({program_name}): {line}");
                                }
                                Ok(None) => break,
                                Err(error) => {
                                    warn!("Failed to read MCP server stderr ({program_name}): {error}");
                                    break;
                                }
                            }
                        },
                    }
                }
            }));
            stop_tx
        });
        let process = StdioServerProcessHandle::local(terminator, stderr_reader);

        Ok(StdioServerTransport {
            inner: transport,
            process,
        })
    }
}

impl LocalProcessTerminator {
    #[cfg(not(windows))]
    fn new(process_group_id: u32) -> Self {
        #[cfg(unix)]
        {
            Self { process_group_id }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = process_group_id;
            Self
        }
    }

    #[cfg(unix)]
    fn terminate(&self) {
        let process_group_id = self.process_group_id;
        let should_escalate = match terminate_process_group(process_group_id) {
            Ok(exists) => exists,
            Err(error) => {
                warn!("Failed to terminate MCP process group {process_group_id}: {error}");
                false
            }
        };
        if should_escalate {
            spawn(move || {
                sleep(PROCESS_GROUP_TERM_GRACE_PERIOD);
                if let Err(error) = kill_process_group(process_group_id) {
                    warn!("Failed to kill MCP process group {process_group_id}: {error}");
                }
            });
        }
    }

    #[cfg(windows)]
    fn terminate(&self) {
        let result = match self {
            Self::Job(job) => job.terminate(),
            Self::Process(process_handle) => {
                codex_utils_pty::JobObject::terminate_process_handle(process_handle)
            }
        };
        if let Err(error) = result {
            warn!("Failed to terminate Windows MCP process: {error}");
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn terminate(&self) {}
}

impl StdioServerProcessHandle {
    fn local(
        terminator: Option<LocalProcessTerminator>,
        stderr_reader: Option<watch::Sender<()>>,
    ) -> Self {
        Self {
            inner: Arc::new(StdioServerProcessHandleInner {
                terminator,
                terminated: AtomicBool::new(false),
                stderr_reader,
            }),
        }
    }

    pub(crate) async fn terminate(&self) -> io::Result<()> {
        if self.inner.terminated.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        if let Some(terminator) = &self.inner.terminator {
            terminator.terminate();
        }
        if let Some(stderr_reader) = &self.inner.stderr_reader {
            stderr_reader.send_replace(());
        }
        Ok(())
    }
}

impl Drop for StdioServerProcessHandleInner {
    fn drop(&mut self) {
        if self.terminated.swap(true, Ordering::AcqRel) {
            return;
        }

        if let Some(terminator) = &self.terminator {
            terminator.terminate();
        }
        if let Some(stderr_reader) = &self.stderr_reader {
            stderr_reader.send_replace(());
        }
    }
}

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;
use std::time::Instant;

use async_channel::Sender;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::spawn::SpawnChildRequest;
use crate::spawn::StdioPolicy;
use crate::spawn::spawn_child_async;
use codex_protocol::error::CodexErr;
use codex_protocol::error::ExecErr;
use codex_protocol::error::Result;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecOutputStream;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use codex_utils_pty::process_group::kill_child_process_group;
use codex_utils_pty::process_group::kill_process_group;
use codex_utils_pty::process_group::terminate_process_group;

pub const DEFAULT_EXEC_COMMAND_TIMEOUT_MS: u64 = 10_000;

// Hardcode these since it does not seem worth including the libc crate just
// for these.
const SIGKILL_CODE: i32 = 9;
const TIMEOUT_CODE: i32 = 64;
const EXIT_CODE_SIGNAL_BASE: i32 = 128; // conventional shell: 128 + signal
const EXEC_TIMEOUT_EXIT_CODE: i32 = 124; // conventional timeout exit code
const CANCELLATION_TERMINATION_GRACE_PERIOD: Duration = Duration::from_millis(50);

// I/O buffer sizing
const READ_CHUNK_SIZE: usize = 8192; // bytes per read
const AGGREGATE_BUFFER_INITIAL_CAPACITY: usize = 8 * 1024; // 8 KiB

/// Hard cap on bytes retained from exec stdout/stderr/aggregated output.
///
/// This mirrors unified exec's output cap so a single runaway command cannot
/// OOM the process by dumping huge amounts of data to stdout/stderr.
const EXEC_OUTPUT_MAX_BYTES: usize = DEFAULT_OUTPUT_BYTES_CAP;

/// Limit the number of ExecCommandOutputDelta events emitted per exec call.
/// Aggregation still collects full output; only the live event stream is capped.
pub(crate) const MAX_EXEC_OUTPUT_DELTAS_PER_CALL: usize = 10_000;

// Wait for the stdout/stderr collection tasks but guard against them
// hanging forever. In the normal case, both pipes are closed once the child
// terminates so the tasks exit quickly. However, if the child process
// spawned grandchildren that inherited its stdout/stderr file descriptors
// those pipes may stay open after we `kill` the direct child on timeout.
// That would cause the `read_capped` tasks to block on `read()`
// indefinitely, effectively hanging the whole agent.
pub const IO_DRAIN_TIMEOUT_MS: u64 = 2_000; // 2 s should be plenty for local pipes

#[derive(Debug)]
pub struct ExecParams {
    pub command: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub expiration: ExecExpiration,
    pub capture_policy: ExecCapturePolicy,
    pub env: HashMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecCapturePolicy {
    /// Shell-like execs keep the historical output cap and timeout behavior.
    #[default]
    ShellTool,
    /// Trusted internal helpers can buffer the full child output in memory
    /// without the shell-oriented output cap or exec-expiration behavior.
    FullBuffer,
    /// Internal helpers that need complete output but must honor timeout and cancellation.
    FullBufferWithExpiration,
}

#[derive(Debug)]
pub struct ExecOptions {
    pub expiration: ExecExpiration,
    pub capture_policy: ExecCapturePolicy,
}

/// Environment handed to a remote exec-server, which resolves it against its own process env.
#[derive(Clone, Debug)]
pub(crate) struct ExecServerEnvConfig {
    pub(crate) policy: codex_exec_server::ExecEnvPolicy,
    pub(crate) local_policy_env: HashMap<String, String>,
}

/// A fully resolved command launch.
#[derive(Debug)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub cwd: PathUri,
    pub env: HashMap<String, String>,
    pub(crate) exec_server_env_config: Option<ExecServerEnvConfig>,
    pub(crate) exec_server_shell_snapshot: Option<codex_exec_server::ShellSnapshotRequest>,
    pub expiration: ExecExpiration,
    pub capture_policy: ExecCapturePolicy,
}

impl ExecRequest {
    pub fn new(
        command: Vec<String>,
        cwd: AbsolutePathBuf,
        env: HashMap<String, String>,
        options: ExecOptions,
    ) -> Self {
        let ExecOptions {
            expiration,
            capture_policy,
        } = options;
        Self {
            command,
            cwd: PathUri::from_abs_path(&cwd),
            env,
            exec_server_env_config: None,
            exec_server_shell_snapshot: None,
            expiration,
            capture_policy,
        }
    }
}

/// Mechanism to terminate an exec invocation before it finishes naturally.
#[derive(Clone, Debug)]
pub enum ExecExpiration {
    Timeout(Duration),
    DefaultTimeout,
    Cancellation(CancellationToken),
    TimeoutOrCancellation {
        timeout: Duration,
        cancellation: CancellationToken,
    },
}

/// Why an `ExecExpiration` completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecExpirationOutcome {
    /// The configured timeout elapsed.
    TimedOut,
    /// The cancellation token was cancelled.
    Cancelled,
}

impl From<u64> for ExecExpiration {
    fn from(timeout_ms: u64) -> Self {
        ExecExpiration::Timeout(Duration::from_millis(timeout_ms))
    }
}

impl ExecExpiration {
    /// Waits for this expiration and reports whether it timed out or was cancelled.
    pub async fn wait_with_outcome(self) -> ExecExpirationOutcome {
        match self {
            ExecExpiration::Timeout(duration) => {
                tokio::time::sleep(duration).await;
                ExecExpirationOutcome::TimedOut
            }
            ExecExpiration::DefaultTimeout => {
                tokio::time::sleep(Duration::from_millis(DEFAULT_EXEC_COMMAND_TIMEOUT_MS)).await;
                ExecExpirationOutcome::TimedOut
            }
            ExecExpiration::Cancellation(cancel) => {
                cancel.cancelled().await;
                ExecExpirationOutcome::Cancelled
            }
            ExecExpiration::TimeoutOrCancellation {
                timeout,
                cancellation,
            } => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => ExecExpirationOutcome::Cancelled,
                    _ = tokio::time::sleep(timeout) => ExecExpirationOutcome::TimedOut,
                }
            }
        }
    }
}

impl ExecCapturePolicy {
    fn retained_bytes_cap(self) -> Option<usize> {
        match self {
            Self::ShellTool => Some(EXEC_OUTPUT_MAX_BYTES),
            Self::FullBuffer | Self::FullBufferWithExpiration => None,
        }
    }

    fn io_drain_timeout(self) -> Duration {
        Duration::from_millis(IO_DRAIN_TIMEOUT_MS)
    }

    fn uses_expiration(self) -> bool {
        match self {
            Self::ShellTool | Self::FullBufferWithExpiration => true,
            Self::FullBuffer => false,
        }
    }
}

#[derive(Clone)]
pub struct StdoutStream {
    pub sub_id: String,
    pub call_id: String,
    pub tx_event: Sender<Event>,
}

pub async fn process_exec_tool_call(
    params: ExecParams,
    stdout_stream: Option<StdoutStream>,
) -> Result<ExecToolCallOutput> {
    execute_exec_request(
        build_exec_request(params),
        stdout_stream,
        /*after_spawn*/ None,
    )
    .await
}

/// Resolve a portable exec request into the concrete argv/env to spawn.
pub fn build_exec_request(params: ExecParams) -> ExecRequest {
    let ExecParams {
        command,
        cwd,
        expiration,
        capture_policy,
        env,
    } = params;
    ExecRequest::new(
        command,
        cwd,
        env,
        ExecOptions {
            expiration,
            capture_policy,
        },
    )
}

pub async fn execute_env(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
) -> Result<ExecToolCallOutput> {
    execute_exec_request(exec_request, stdout_stream, /*after_spawn*/ None).await
}

pub(crate) async fn execute_exec_request(
    exec_request: ExecRequest,
    stdout_stream: Option<StdoutStream>,
    after_spawn: Option<Box<dyn FnOnce() + Send>>,
) -> Result<ExecToolCallOutput> {
    let ExecRequest {
        command,
        cwd,
        env,
        exec_server_env_config: _,
        exec_server_shell_snapshot: _,
        expiration,
        capture_policy,
    } = exec_request;

    // TODO(anp): Keep PathUri through the local process launch boundary.
    let cwd = cwd
        .to_abs_path()
        .map_err(|err| CodexErr::InvalidRequest(format!("invalid exec cwd: {err}")))?;

    let params = ExecParams {
        command,
        cwd,
        expiration,
        capture_policy,
        env,
    };

    let start = Instant::now();
    let raw_output_result = exec(params, stdout_stream, after_spawn).await;
    let duration = start.elapsed();
    finalize_exec_result(raw_output_result, duration)
}

fn finalize_exec_result(
    raw_output_result: std::result::Result<RawExecToolCallOutput, CodexErr>,
    duration: Duration,
) -> Result<ExecToolCallOutput> {
    match raw_output_result {
        Ok(raw_output) => {
            #[allow(unused_mut)]
            let mut timed_out = raw_output.timed_out;

            #[cfg(target_family = "unix")]
            {
                if let Some(signal) = raw_output.exit_status.signal() {
                    if signal == TIMEOUT_CODE {
                        timed_out = true;
                    } else {
                        return Err(CodexErr::from(ExecErr::Signal(signal)));
                    }
                }
            }

            let mut exit_code = raw_output.exit_status.code().unwrap_or(-1);
            if timed_out {
                exit_code = EXEC_TIMEOUT_EXIT_CODE;
            }

            let stdout = raw_output.stdout.from_utf8_lossy();
            let stderr = raw_output.stderr.from_utf8_lossy();
            let aggregated_output = raw_output.aggregated_output.from_utf8_lossy();
            let exec_output = ExecToolCallOutput {
                exit_code,
                stdout,
                stderr,
                aggregated_output,
                duration,
                timed_out,
            };

            if timed_out {
                return Err(CodexErr::from(ExecErr::Timeout {
                    output: Box::new(exec_output),
                }));
            }

            Ok(exec_output)
        }
        Err(err) => {
            tracing::error!("exec error: {err}");
            Err(err)
        }
    }
}

#[derive(Debug)]
struct RawExecToolCallOutput {
    pub exit_status: ExitStatus,
    pub stdout: StreamOutput<Vec<u8>>,
    pub stderr: StreamOutput<Vec<u8>>,
    pub aggregated_output: StreamOutput<Vec<u8>>,
    pub timed_out: bool,
}

#[inline]
fn append_capped(dst: &mut Vec<u8>, src: &[u8], max_bytes: usize) {
    if dst.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes.saturating_sub(dst.len());
    let take = remaining.min(src.len());
    dst.extend_from_slice(&src[..take]);
}

fn aggregate_output(
    stdout: &StreamOutput<Vec<u8>>,
    stderr: &StreamOutput<Vec<u8>>,
    max_bytes: Option<usize>,
) -> StreamOutput<Vec<u8>> {
    let Some(max_bytes) = max_bytes else {
        let total_len = stdout.text.len().saturating_add(stderr.text.len());
        let mut aggregated = Vec::with_capacity(total_len);
        aggregated.extend_from_slice(&stdout.text);
        aggregated.extend_from_slice(&stderr.text);
        return StreamOutput {
            text: aggregated,
            truncated_after_lines: None,
        };
    };

    let total_len = stdout.text.len().saturating_add(stderr.text.len());
    let mut aggregated = Vec::with_capacity(total_len.min(max_bytes));

    if total_len <= max_bytes {
        aggregated.extend_from_slice(&stdout.text);
        aggregated.extend_from_slice(&stderr.text);
        return StreamOutput {
            text: aggregated,
            truncated_after_lines: None,
        };
    }

    // Under contention, reserve 1/3 for stdout and 2/3 for stderr; rebalance unused stderr to stdout.
    let want_stdout = stdout.text.len().min(max_bytes / 3);
    let want_stderr = stderr.text.len();
    let stderr_take = want_stderr.min(max_bytes.saturating_sub(want_stdout));
    let remaining = max_bytes.saturating_sub(want_stdout + stderr_take);
    let stdout_take = want_stdout + remaining.min(stdout.text.len().saturating_sub(want_stdout));

    aggregated.extend_from_slice(&stdout.text[..stdout_take]);
    aggregated.extend_from_slice(&stderr.text[..stderr_take]);

    StreamOutput {
        text: aggregated,
        truncated_after_lines: None,
    }
}

/// Executes a command specified by [ExecParams]. Events are reported via
/// `stdout_stream`, if specified, and `after_spawn` is invoked once the child
/// process has been spawned, before output consumption begins.
async fn exec(
    params: ExecParams,
    stdout_stream: Option<StdoutStream>,
    after_spawn: Option<Box<dyn FnOnce() + Send>>,
) -> Result<RawExecToolCallOutput> {
    let ExecParams {
        command,
        cwd,
        env,
        expiration,
        capture_policy,
    } = params;

    let (program, args) = command.split_first().ok_or_else(|| {
        CodexErr::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command args are empty",
        ))
    })?;
    let child = spawn_child_async(SpawnChildRequest {
        program: PathBuf::from(program),
        args: args.into(),
        cwd,
        stdio_policy: StdioPolicy::RedirectForShellTool,
        env,
    })
    .await?;
    if let Some(after_spawn) = after_spawn {
        after_spawn();
    }
    consume_output(child, expiration, capture_policy, stdout_stream).await
}

/// Consumes the output of a child process according to the configured capture
/// policy.
async fn consume_output(
    mut child: Child,
    expiration: ExecExpiration,
    capture_policy: ExecCapturePolicy,
    stdout_stream: Option<StdoutStream>,
) -> Result<RawExecToolCallOutput> {
    // Both stdout and stderr were configured with `Stdio::piped()`
    // above, therefore `take()` should normally return `Some`.  If it doesn't
    // we treat it as an exceptional I/O error

    let stdout_reader = child.stdout.take().ok_or_else(|| {
        CodexErr::Io(io::Error::other(
            "stdout pipe was unexpectedly not available",
        ))
    })?;
    let stderr_reader = child.stderr.take().ok_or_else(|| {
        CodexErr::Io(io::Error::other(
            "stderr pipe was unexpectedly not available",
        ))
    })?;

    let retained_bytes_cap = capture_policy.retained_bytes_cap();
    let stdout_handle = tokio::spawn(read_output(
        BufReader::new(stdout_reader),
        stdout_stream.clone(),
        /*is_stderr*/ false,
        retained_bytes_cap,
    ));
    let stderr_handle = tokio::spawn(read_output(
        BufReader::new(stderr_reader),
        stdout_stream.clone(),
        /*is_stderr*/ true,
        retained_bytes_cap,
    ));

    let expiration_wait = async {
        if capture_policy.uses_expiration() {
            Some(expiration.wait_with_outcome().await)
        } else {
            std::future::pending::<Option<ExecExpirationOutcome>>().await
        }
    };
    tokio::pin!(expiration_wait);
    let process_group_id = child.id();
    let mut expiration_resolved = false;
    let (mut exit_status, mut timed_out) = tokio::select! {
        status_result = child.wait() => {
            let exit_status = status_result?;
            (exit_status, false)
        }
        outcome = &mut expiration_wait => {
            expiration_resolved = true;
            match outcome {
                Some(ExecExpirationOutcome::TimedOut) => {
                    kill_child_process_group(&mut child)?;
                    child.start_kill()?;
                    (
                        synthetic_exit_status(EXIT_CODE_SIGNAL_BASE + TIMEOUT_CODE),
                        true,
                    )
                }
                Some(ExecExpirationOutcome::Cancelled) => {
                    // Let TERM-aware processes run cleanup briefly, then kill any
                    // remaining members of the original process group.
                    let process_group_id = child.id();
                    let should_escalate = if let Some(process_group_id) = process_group_id {
                        terminate_process_group(process_group_id)?
                    } else {
                        false
                    };
                    match tokio::time::timeout(
                        CANCELLATION_TERMINATION_GRACE_PERIOD,
                        child.wait(),
                    )
                    .await
                    {
                        Ok(status) => {
                            status?;
                            if should_escalate
                                && let Some(process_group_id) = process_group_id
                            {
                                kill_process_group(process_group_id)?;
                            }
                        }
                        Err(_) => {
                            if let Some(process_group_id) = process_group_id {
                                kill_process_group(process_group_id)?;
                            }
                            child.start_kill()?;
                        }
                    }
                    (synthetic_exit_status_for_code(/*code*/ 1), false)
                }
                None => unreachable!("expiration wait only resolves while expiration is active"),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            kill_child_process_group(&mut child)?;
            child.start_kill()?;
            (synthetic_exit_status(EXIT_CODE_SIGNAL_BASE + SIGKILL_CODE), false)
        }
    };

    // We need mutable bindings so we can `abort()` them on timeout.
    use tokio::task::JoinHandle;

    async fn await_output(
        handle: &mut JoinHandle<std::io::Result<StreamOutput<Vec<u8>>>>,
        timeout: Duration,
    ) -> std::io::Result<StreamOutput<Vec<u8>>> {
        match tokio::time::timeout(timeout, &mut *handle).await {
            Ok(join_res) => match join_res {
                Ok(io_res) => io_res,
                Err(join_err) => Err(std::io::Error::other(join_err)),
            },
            Err(_elapsed) => {
                // Timeout: abort the task to avoid hanging on open pipes.
                handle.abort();
                Ok(StreamOutput {
                    text: Vec::new(),
                    truncated_after_lines: None,
                })
            }
        }
    }

    let mut stdout_handle = stdout_handle;
    let mut stderr_handle = stderr_handle;

    let (stdout, stderr) = if matches!(capture_policy, ExecCapturePolicy::FullBufferWithExpiration)
    {
        let mut stdout_done = false;
        let mut stderr_done = false;
        let drain = async {
            let stdout = (&mut stdout_handle).await;
            stdout_done = true;
            let stdout = stdout.map_err(io::Error::other)??;
            let stderr = (&mut stderr_handle).await;
            stderr_done = true;
            let stderr = stderr.map_err(io::Error::other)??;
            Ok::<_, io::Error>((stdout, stderr))
        };
        // The leader can exit while descendants still hold its pipes. Keep the original
        // expiration active until draining finishes, without polling a completed future again.
        let drained = tokio::select! {
            biased;
            outcome = &mut expiration_wait, if !expiration_resolved => {
                match outcome {
                    Some(ExecExpirationOutcome::TimedOut) => {
                        exit_status = synthetic_exit_status(EXIT_CODE_SIGNAL_BASE + TIMEOUT_CODE);
                        timed_out = true;
                    }
                    Some(ExecExpirationOutcome::Cancelled) => {
                        exit_status = synthetic_exit_status_for_code(/*code*/ 1);
                    }
                    None => unreachable!("full-buffer capture always uses expiration"),
                }
                None
            }
            result = tokio::time::timeout(capture_policy.io_drain_timeout(), drain) => {
                Some(result.unwrap_or_else(|_| Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "capture output pipes did not close before the drain deadline",
                ))))
            }
        };
        match drained {
            Some(Ok(output)) => output,
            failure => {
                let cleanup = process_group_id.map_or(Ok(()), kill_process_group);
                stdout_handle.abort();
                stderr_handle.abort();
                if !stdout_done {
                    let _ = stdout_handle.await;
                }
                if !stderr_done {
                    let _ = stderr_handle.await;
                }
                cleanup?;
                if let Some(Err(err)) = failure {
                    return Err(err.into());
                }
                (
                    StreamOutput {
                        text: Vec::new(),
                        truncated_after_lines: None,
                    },
                    StreamOutput {
                        text: Vec::new(),
                        truncated_after_lines: None,
                    },
                )
            }
        }
    } else {
        let stdout = await_output(&mut stdout_handle, capture_policy.io_drain_timeout()).await?;
        let stderr = await_output(&mut stderr_handle, capture_policy.io_drain_timeout()).await?;
        (stdout, stderr)
    };
    let aggregated_output = aggregate_output(&stdout, &stderr, retained_bytes_cap);

    Ok(RawExecToolCallOutput {
        exit_status,
        stdout,
        stderr,
        aggregated_output,
        timed_out,
    })
}

async fn read_output<R: AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    stream: Option<StdoutStream>,
    is_stderr: bool,
    max_bytes: Option<usize>,
) -> io::Result<StreamOutput<Vec<u8>>> {
    let mut buf = Vec::with_capacity(
        max_bytes.map_or(AGGREGATE_BUFFER_INITIAL_CAPACITY, |max_bytes| {
            AGGREGATE_BUFFER_INITIAL_CAPACITY.min(max_bytes)
        }),
    );
    let mut tmp = [0u8; READ_CHUNK_SIZE];
    let mut emitted_deltas: usize = 0;

    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }

        if let Some(stream) = &stream
            && emitted_deltas < MAX_EXEC_OUTPUT_DELTAS_PER_CALL
        {
            let chunk = tmp[..n].to_vec();
            let msg = EventMsg::ExecCommandOutputDelta(ExecCommandOutputDeltaEvent {
                call_id: stream.call_id.clone(),
                stream: if is_stderr {
                    ExecOutputStream::Stderr
                } else {
                    ExecOutputStream::Stdout
                },
                chunk,
            });
            let event = Event {
                id: stream.sub_id.clone(),
                msg,
            };
            #[allow(clippy::let_unit_value)]
            let _ = stream.tx_event.send(event).await;
            emitted_deltas += 1;
        }

        if let Some(max_bytes) = max_bytes {
            append_capped(&mut buf, &tmp[..n], max_bytes);
        } else {
            buf.extend_from_slice(&tmp[..n]);
        }
        // Continue reading to EOF to avoid back-pressure
    }

    Ok(StreamOutput {
        text: buf,
        truncated_after_lines: None,
    })
}

#[cfg(unix)]
fn synthetic_exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code)
}

#[cfg(unix)]
fn synthetic_exit_status_for_code(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn synthetic_exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    // On Windows the raw status is a u32. Use a direct cast to avoid
    // panicking on negative i32 values produced by prior narrowing casts.
    std::process::ExitStatus::from_raw(code as u32)
}

#[cfg(windows)]
fn synthetic_exit_status_for_code(code: i32) -> ExitStatus {
    synthetic_exit_status(code)
}

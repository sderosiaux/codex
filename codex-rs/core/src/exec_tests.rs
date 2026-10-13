use super::*;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::time::Duration;
use test_case::test_case;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

#[tokio::test]
async fn read_output_limits_retained_bytes_for_shell_capture() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let bytes = vec![b'a'; EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024)];
    tokio::spawn(async move {
        writer.write_all(&bytes).await.expect("write");
    });

    let out = read_output(
        reader,
        /*stream*/ None,
        /*is_stderr*/ false,
        Some(EXEC_OUTPUT_MAX_BYTES),
    )
    .await
    .expect("read");
    assert_eq!(out.text.len(), EXEC_OUTPUT_MAX_BYTES);
}

#[test]
fn aggregate_output_prefers_stderr_on_contention() {
    let stdout = StreamOutput {
        text: vec![b'a'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };
    let stderr = StreamOutput {
        text: vec![b'b'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };

    let aggregated = aggregate_output(&stdout, &stderr, Some(EXEC_OUTPUT_MAX_BYTES));
    let stdout_cap = EXEC_OUTPUT_MAX_BYTES / 3;
    let stderr_cap = EXEC_OUTPUT_MAX_BYTES.saturating_sub(stdout_cap);

    assert_eq!(aggregated.text.len(), EXEC_OUTPUT_MAX_BYTES);
    assert_eq!(aggregated.text[..stdout_cap], vec![b'a'; stdout_cap]);
    assert_eq!(aggregated.text[stdout_cap..], vec![b'b'; stderr_cap]);
}

#[test]
fn aggregate_output_fills_remaining_capacity_with_stderr() {
    let stdout_len = EXEC_OUTPUT_MAX_BYTES / 10;
    let stdout = StreamOutput {
        text: vec![b'a'; stdout_len],
        truncated_after_lines: None,
    };
    let stderr = StreamOutput {
        text: vec![b'b'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };

    let aggregated = aggregate_output(&stdout, &stderr, Some(EXEC_OUTPUT_MAX_BYTES));
    let stderr_cap = EXEC_OUTPUT_MAX_BYTES.saturating_sub(stdout_len);

    assert_eq!(aggregated.text.len(), EXEC_OUTPUT_MAX_BYTES);
    assert_eq!(aggregated.text[..stdout_len], vec![b'a'; stdout_len]);
    assert_eq!(aggregated.text[stdout_len..], vec![b'b'; stderr_cap]);
}

#[test]
fn aggregate_output_rebalances_when_stderr_is_small() {
    let stdout = StreamOutput {
        text: vec![b'a'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };
    let stderr = StreamOutput {
        text: vec![b'b'; 1],
        truncated_after_lines: None,
    };

    let aggregated = aggregate_output(&stdout, &stderr, Some(EXEC_OUTPUT_MAX_BYTES));
    let stdout_len = EXEC_OUTPUT_MAX_BYTES.saturating_sub(1);

    assert_eq!(aggregated.text.len(), EXEC_OUTPUT_MAX_BYTES);
    assert_eq!(aggregated.text[..stdout_len], vec![b'a'; stdout_len]);
    assert_eq!(aggregated.text[stdout_len..], vec![b'b'; 1]);
}

#[test]
fn aggregate_output_keeps_stdout_then_stderr_when_under_cap() {
    let stdout = StreamOutput {
        text: vec![b'a'; 4],
        truncated_after_lines: None,
    };
    let stderr = StreamOutput {
        text: vec![b'b'; 3],
        truncated_after_lines: None,
    };

    let aggregated = aggregate_output(&stdout, &stderr, Some(EXEC_OUTPUT_MAX_BYTES));
    let mut expected = Vec::new();
    expected.extend_from_slice(&stdout.text);
    expected.extend_from_slice(&stderr.text);

    assert_eq!(aggregated.text, expected);
    assert_eq!(aggregated.truncated_after_lines, None);
}

#[tokio::test]
async fn read_output_retains_all_bytes_for_full_buffer_capture() {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let bytes = vec![b'a'; EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024)];
    let expected_len = bytes.len();
    // The duplex pipe is smaller than `bytes`, so the writer must run concurrently
    // with `read_output()` or `write_all()` will block once the buffer fills up.
    tokio::spawn(async move {
        writer.write_all(&bytes).await.expect("write");
    });

    let out = read_output(
        reader, /*stream*/ None, /*is_stderr*/ false, /*max_bytes*/ None,
    )
    .await
    .expect("read");
    assert_eq!(out.text.len(), expected_len);
}

#[test]
fn aggregate_output_keeps_all_bytes_when_uncapped() {
    let stdout = StreamOutput {
        text: vec![b'a'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };
    let stderr = StreamOutput {
        text: vec![b'b'; EXEC_OUTPUT_MAX_BYTES],
        truncated_after_lines: None,
    };

    let aggregated = aggregate_output(&stdout, &stderr, /*max_bytes*/ None);

    assert_eq!(aggregated.text.len(), EXEC_OUTPUT_MAX_BYTES * 2);
    assert_eq!(
        aggregated.text[..EXEC_OUTPUT_MAX_BYTES],
        vec![b'a'; EXEC_OUTPUT_MAX_BYTES]
    );
    assert_eq!(
        aggregated.text[EXEC_OUTPUT_MAX_BYTES..],
        vec![b'b'; EXEC_OUTPUT_MAX_BYTES]
    );
}

#[test]
fn full_buffer_capture_policy_disables_caps_and_exec_expiration() {
    assert_eq!(ExecCapturePolicy::FullBuffer.retained_bytes_cap(), None);
    assert_eq!(
        ExecCapturePolicy::FullBuffer.io_drain_timeout(),
        Duration::from_millis(IO_DRAIN_TIMEOUT_MS)
    );
    assert!(!ExecCapturePolicy::FullBuffer.uses_expiration());
}

#[tokio::test]
async fn exec_full_buffer_capture_ignores_expiration() -> Result<()> {
    #[cfg(windows)]
    let command = vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Milliseconds 50; [Console]::Out.Write('hello')".to_string(),
    ];
    #[cfg(not(windows))]
    let command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 0.05; printf hello".to_string(),
    ];

    let env: HashMap<String, String> = std::env::vars().collect();
    let output = exec(
        ExecParams {
            command,
            cwd: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
            expiration: 1.into(),
            capture_policy: ExecCapturePolicy::FullBuffer,
            env,
        },
        /*stdout_stream*/ None,
        /*after_spawn*/ None,
    )
    .await?;

    assert_eq!(output.stdout.from_utf8_lossy().text.trim(), "hello");
    assert!(!output.timed_out);

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn exec_full_buffer_capture_keeps_io_drain_timeout_when_descendant_holds_pipe_open()
-> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_millis(IO_DRAIN_TIMEOUT_MS * 3),
        exec(
            ExecParams {
                command: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "printf hello; sleep 30 &".to_string(),
                ],
                cwd: codex_utils_absolute_path::AbsolutePathBuf::current_dir()?,
                expiration: 1.into(),
                capture_policy: ExecCapturePolicy::FullBuffer,
                env: std::env::vars().collect(),
            },
            /*stdout_stream*/ None,
            /*after_spawn*/ None,
        ),
    )
    .await
    .expect("full-buffer exec should return once the I/O drain guard fires")?;

    assert!(!output.timed_out);

    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum CaptureDrainFailure {
    Cancellation,
    Timeout,
    DrainTimeout,
}

#[cfg(unix)]
#[test_case("stdout", CaptureDrainFailure::Cancellation, ExecCapturePolicy::FullBufferWithExpiration; "cancel_stdout")]
#[test_case("stderr", CaptureDrainFailure::Cancellation, ExecCapturePolicy::FullBufferWithExpiration; "cancel_stderr")]
#[test_case("stdout", CaptureDrainFailure::Timeout, ExecCapturePolicy::FullBufferWithExpiration; "timeout_stdout")]
#[test_case("stderr", CaptureDrainFailure::Timeout, ExecCapturePolicy::FullBufferWithExpiration; "timeout_stderr")]
#[test_case("stdout", CaptureDrainFailure::DrainTimeout, ExecCapturePolicy::FullBufferWithExpiration; "incomplete_stdout")]
#[test_case("stderr", CaptureDrainFailure::DrainTimeout, ExecCapturePolicy::FullBufferWithExpiration; "incomplete_stderr")]
#[tokio::test]
async fn full_buffer_expiration_cleans_up_after_leader_exit(
    pipe: &str,
    failure: CaptureDrainFailure,
    capture_policy: ExecCapturePolicy,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let redirect = match pipe {
        "stdout" => "2>/dev/null",
        "stderr" => ">/dev/null",
        _ => unreachable!("test specifies stdout or stderr"),
    };
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", &format!(
            "printf complete; (while [ ! -f release ]; do sleep 0.01; done; printf survived > late_write) {redirect} &"
        )])
        .current_dir(dir.path())
        .process_group(/*pgroup*/ 0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn()?;
    let process_group_id = child.id().expect("running child");
    let cancellation = CancellationToken::new();
    let expiration = match failure {
        CaptureDrainFailure::Cancellation => ExecExpiration::Cancellation(cancellation.clone()),
        CaptureDrainFailure::Timeout => ExecExpiration::Timeout(Duration::from_millis(250)),
        CaptureDrainFailure::DrainTimeout => ExecExpiration::Timeout(Duration::from_secs(30)),
    };
    let capture = consume_output(
        child,
        expiration,
        capture_policy,
        /*stdout_stream*/ None,
    );
    let after_leader_exit = async {
        timeout(Duration::from_secs(5), async {
            while unsafe {
                libc::kill(process_group_id as libc::pid_t, /*sig*/ 0)
            } == 0
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("capture should reap the direct child before cancellation");
        if matches!(failure, CaptureDrainFailure::Cancellation) {
            cancellation.cancel();
        }
    };
    let (result, ()) = timeout(Duration::from_secs(6), async {
        tokio::join!(capture, after_leader_exit)
    })
    .await?;
    std::fs::write(dir.path().join("release"), "")?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let survived = dir.path().join("late_write").exists();
    codex_utils_pty::process_group::kill_process_group(process_group_id)?;
    assert!(
        !survived,
        "pipe-holding descendant survived capture failure"
    );
    match failure {
        CaptureDrainFailure::Cancellation | CaptureDrainFailure::Timeout => {
            let output = result?;
            assert_eq!(
                (
                    output.timed_out,
                    output.exit_status.success(),
                    output.stdout.text,
                    output.stderr.text
                ),
                (
                    matches!(failure, CaptureDrainFailure::Timeout),
                    false,
                    Vec::new(),
                    Vec::new()
                ),
            );
        }
        CaptureDrainFailure::DrainTimeout => {
            assert!(result.is_err(), "incomplete capture must fail");
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn full_buffer_expiration_preserves_successful_background_startup() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            "printf complete; (sleep 0.05; printf survived > late_write) >/dev/null 2>&1 &",
        ])
        .current_dir(dir.path())
        .process_group(/*pgroup*/ 0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = consume_output(
        command.spawn()?,
        ExecExpiration::Timeout(Duration::from_secs(5)),
        ExecCapturePolicy::FullBufferWithExpiration,
        /*stdout_stream*/ None,
    )
    .await?;
    assert!(output.exit_status.success());
    assert_eq!(output.stdout.text, b"complete");
    timeout(Duration::from_secs(5), async {
        while !dir.path().join("late_write").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

#[test_case(ExecCapturePolicy::FullBuffer, 1; "without_expiration")]
#[test_case(ExecCapturePolicy::FullBufferWithExpiration, 30_000; "with_expiration")]
#[tokio::test]
async fn process_exec_tool_call_preserves_full_buffer_capture_policy(
    capture_policy: ExecCapturePolicy,
    timeout_ms: u64,
) -> Result<()> {
    let byte_count = EXEC_OUTPUT_MAX_BYTES.saturating_add(128 * 1024);
    #[cfg(windows)]
    let command = vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        format!("Start-Sleep -Milliseconds 50; [Console]::Out.Write('a' * {byte_count})"),
    ];
    #[cfg(not(windows))]
    let command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        format!("sleep 0.05; head -c {byte_count} /dev/zero | tr '\\0' 'a'"),
    ];

    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let output = process_exec_tool_call(
        ExecParams {
            command,
            cwd: cwd.clone(),
            expiration: timeout_ms.into(),
            capture_policy,
            env: std::env::vars().collect(),
        },
        /*stdout_stream*/ None,
    )
    .await?;

    assert!(!output.timed_out);
    assert_eq!(output.stdout.text.len(), byte_count);

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn kill_child_process_group_kills_grandchildren_on_timeout() -> Result<()> {
    // On Linux/macOS, /bin/bash is typically present; on FreeBSD/OpenBSD,
    // prefer /bin/sh to avoid NotFound errors.
    #[cfg(any(target_os = "freebsd", target_os = "openbsd"))]
    let command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 60 & echo $!; sleep 60".to_string(),
    ];
    #[cfg(all(unix, not(any(target_os = "freebsd", target_os = "openbsd"))))]
    let command = vec![
        "/bin/bash".to_string(),
        "-c".to_string(),
        "sleep 60 & echo $!; sleep 60".to_string(),
    ];
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let params = ExecParams {
        command,
        cwd,
        expiration: 500.into(),
        capture_policy: ExecCapturePolicy::ShellTool,
        env,
    };

    let output = exec(
        params, /*stdout_stream*/ None, /*after_spawn*/ None,
    )
    .await?;
    assert!(output.timed_out);

    let stdout = output.stdout.from_utf8_lossy().text;
    let pid_line = stdout.lines().next().unwrap_or("").trim();
    let pid: i32 = pid_line.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to parse pid from stdout '{pid_line}': {error}"),
        )
    })?;

    let mut killed = false;
    for _ in 0..20 {
        // Use kill(pid, 0) to check if the process is alive.
        if unsafe { libc::kill(pid, 0) } == -1
            && let Some(libc::ESRCH) = std::io::Error::last_os_error().raw_os_error()
        {
            killed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(killed, "grandchild process with pid {pid} is still alive");
    Ok(())
}

#[tokio::test]
async fn process_exec_tool_call_respects_cancellation_token() -> Result<()> {
    let command = long_running_command();
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let cancel_token = CancellationToken::new();
    let cancel_tx = cancel_token.clone();
    let params = ExecParams {
        command,
        cwd: cwd.clone(),
        expiration: ExecExpiration::Cancellation(cancel_token),
        capture_policy: ExecCapturePolicy::ShellTool,
        env,
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        cancel_tx.cancel();
    });
    let result = timeout(
        Duration::from_secs(5),
        process_exec_tool_call(params, /*stdout_stream*/ None),
    )
    .await
    .expect("cancellation should stop the process promptly");
    let output = result.expect("cancellation should return a non-timeout exec result");
    assert!(!output.timed_out);
    assert_ne!(output.exit_code, 0);
    assert_ne!(output.exit_code, EXEC_TIMEOUT_EXIT_CODE);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn process_exec_tool_call_cancellation_allows_sigterm_cleanup() -> Result<()> {
    let temp_dir = tempfile::TempDir::new()?;
    let ready_marker = temp_dir.path().join("ready");
    let cleanup_marker = temp_dir.path().join("cleanup");
    let descendant_pid_marker = temp_dir.path().join("descendant-pid");
    // The parent handles TERM and records cleanup, while a TERM-ignoring child
    // proves cancellation still escalates any survivors in the process group.
    let command = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"(trap '' TERM; sleep 60) &
printf '%s' "$!" > "$DESCENDANT_PID_MARKER"
trap 'printf cleaned > "$CLEANUP_MARKER"; exit 0' TERM
printf ready > "$READY_MARKER"
while :; do sleep 1; done"#
            .to_string(),
    ];
    let cwd = codex_utils_absolute_path::AbsolutePathBuf::current_dir()?;
    let mut env: HashMap<String, String> = std::env::vars().collect();
    env.insert(
        "READY_MARKER".to_string(),
        ready_marker.to_string_lossy().into_owned(),
    );
    env.insert(
        "CLEANUP_MARKER".to_string(),
        cleanup_marker.to_string_lossy().into_owned(),
    );
    env.insert(
        "DESCENDANT_PID_MARKER".to_string(),
        descendant_pid_marker.to_string_lossy().into_owned(),
    );
    let cancel_token = CancellationToken::new();
    let cancel_tx = cancel_token.clone();
    tokio::spawn(async move {
        for _ in 0..50 {
            if ready_marker.exists() {
                cancel_tx.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel_tx.cancel();
    });
    let params = ExecParams {
        command,
        cwd: cwd.clone(),
        expiration: ExecExpiration::TimeoutOrCancellation {
            timeout: Duration::from_millis(DEFAULT_EXEC_COMMAND_TIMEOUT_MS),
            cancellation: cancel_token,
        },
        capture_policy: ExecCapturePolicy::ShellTool,
        env,
    };

    let result = timeout(
        Duration::from_secs(5),
        process_exec_tool_call(params, /*stdout_stream*/ None),
    )
    .await
    .expect("cancellation should stop the process promptly");
    let output = result.expect("cancellation should return a non-timeout exec result");
    assert!(!output.timed_out);
    assert_eq!(
        std::fs::read_to_string(cleanup_marker)?,
        "cleaned",
        "SIGTERM cleanup trap should run before cancellation falls back to a hard kill"
    );
    let descendant_pid = std::fs::read_to_string(descendant_pid_marker)?
        .parse::<i32>()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to parse descendant pid: {error}"),
            )
        })?;
    let mut killed = false;
    for _ in 0..20 {
        if unsafe { libc::kill(descendant_pid, 0) } == -1
            && let Some(libc::ESRCH) = std::io::Error::last_os_error().raw_os_error()
        {
            killed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !killed {
        unsafe {
            libc::kill(descendant_pid, libc::SIGKILL);
        }
    }
    assert!(
        killed,
        "TERM-ignoring descendant process with pid {descendant_pid} is still alive"
    );
    Ok(())
}

#[cfg(unix)]
fn long_running_command() -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "sleep 30".to_string(),
    ]
}

#[cfg(windows)]
fn long_running_command() -> Vec<String> {
    vec![
        "powershell.exe".to_string(),
        "-NonInteractive".to_string(),
        "-NoLogo".to_string(),
        "-Command".to_string(),
        "Start-Sleep -Seconds 30".to_string(),
    ]
}

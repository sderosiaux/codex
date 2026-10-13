use super::*;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Command as StdCommand;

use tempfile::tempdir;

#[cfg(unix)]
struct BlockingStdinPipe {
    original: i32,
    write_end: i32,
}

#[cfg(unix)]
impl BlockingStdinPipe {
    fn install() -> Result<Self> {
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error()).context("create stdin pipe");
        }

        let original = unsafe { libc::dup(libc::STDIN_FILENO) };
        if original == -1 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err).context("dup stdin");
        }

        if unsafe { libc::dup2(fds[0], libc::STDIN_FILENO) } == -1 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
                libc::close(original);
            }
            return Err(err).context("replace stdin");
        }

        unsafe {
            libc::close(fds[0]);
        }

        Ok(Self {
            original,
            write_end: fds[1],
        })
    }
}

#[cfg(unix)]
impl Drop for BlockingStdinPipe {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.original, libc::STDIN_FILENO);
            libc::close(self.original);
            libc::close(self.write_end);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn assert_posix_snapshot_sections(snapshot: &str) {
    assert!(snapshot.contains("# Snapshot file"));
    assert!(snapshot.contains("aliases "));
    assert!(snapshot.contains("exports "));
    assert!(
        snapshot.contains("PATH"),
        "snapshot should capture a PATH export"
    );
    assert!(snapshot.contains("setopts "));
}

async fn get_snapshot(shell_type: ShellType) -> Result<String> {
    let dir = tempdir()?;
    let path = dir.path().join("snapshot.sh");
    let shell = crate::shell::get_shell(shell_type)
        .with_context(|| format!("No available shell for {shell_type:?}"))?;
    write_shell_snapshot(
        &shell,
        &path.abs(),
        &dir.path().abs(),
        &ShellEnvironmentPolicy::default(),
    )
    .await?;
    let content = fs::read_to_string(&path).await?;
    Ok(content)
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_helpers_preserve_thread_without_caching_exec_metadata() -> Result<()> {
    let output = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg("shell_snapshot::tests::snapshot_helper_exec_metadata_child")
        .arg("--nocapture")
        .env("CODEX_TEST_SNAPSHOT_HELPER_CHILD", "1")
        .env("CODEX_THREAD_ID", "helper-thread")
        .env("CODEX_TOOL_CALL_ID", "stale-call")
        .env("BASH_ENV", "/dev/null")
        .output()?;

    assert!(
        output.status.success(),
        "snapshot helper child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_helper_exec_metadata_child() -> Result<()> {
    if std::env::var_os("CODEX_TEST_SNAPSHOT_HELPER_CHILD").is_none() {
        return Ok(());
    }
    assert_eq!(
        std::env::var("CODEX_THREAD_ID").as_deref(),
        Ok("helper-thread")
    );
    assert_eq!(
        std::env::var("CODEX_TOOL_CALL_ID").as_deref(),
        Ok("stale-call")
    );

    let dir = tempdir()?;
    let shell = crate::shell::get_shell(ShellType::Bash).context("No available Bash shell")?;
    let helper_env = run_script_with_timeout(
        &shell,
        "printf '%s|%s' \"${CODEX_THREAD_ID-unset}\" \"${CODEX_TOOL_CALL_ID-unset}\"",
        SNAPSHOT_TIMEOUT,
        /*login_shell*/ false,
        &dir.path().abs(),
    )
    .await?;
    assert_eq!(helper_env, "helper-thread|unset");
    let snapshot = get_snapshot(ShellType::Bash).await?;
    assert!(!snapshot.contains("CODEX_THREAD_ID"));
    assert!(!snapshot.contains("CODEX_TOOL_CALL_ID"));
    assert!(!snapshot.contains("helper-thread"));
    assert!(!snapshot.contains("stale-call"));

    let validation_path = dir.path().join("validation.sh").abs();
    fs::write(
        &validation_path,
        "test \"${CODEX_THREAD_ID-missing}\" = helper-thread && \
         test \"${CODEX_TOOL_CALL_ID-missing}\" = missing\n",
    )
    .await?;
    validate_snapshot(&shell, &validation_path, &dir.path().abs()).await?;
    Ok(())
}

#[test]
fn snapshot_file_name_parser_supports_legacy_and_suffixed_names() {
    let session_id = "019cf82b-6a62-7700-bbbd-46909794ef89";

    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.sh")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.123.sh")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name(&format!("{session_id}.tmp-123")),
        Some(session_id)
    );
    assert_eq!(
        snapshot_session_id_from_file_name("not-a-snapshot.txt"),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_creates_and_deletes_snapshot_file() -> Result<()> {
    let dir = tempdir()?;
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        ThreadId::new(),
        &dir.path().abs(),
        &ShellEnvironmentPolicy::default(),
        &shell,
        /*state_db*/ None,
    )
    .await
    .expect("snapshot should be created");
    let path = snapshot.path.clone();
    assert!(path.exists());

    drop(snapshot);

    assert!(!path.exists());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn try_create_uses_distinct_generation_paths() -> Result<()> {
    let dir = tempdir()?;
    let session_id = ThreadId::new();
    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let initial_snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        session_id,
        &dir.path().abs(),
        &ShellEnvironmentPolicy::default(),
        &shell,
        /*state_db*/ None,
    )
    .await
    .expect("initial snapshot should be created");
    let refreshed_snapshot = ShellSnapshot::try_create(
        &dir.path().abs(),
        session_id,
        &dir.path().abs(),
        &ShellEnvironmentPolicy::default(),
        &shell,
        /*state_db*/ None,
    )
    .await
    .expect("refreshed snapshot should be created");
    let initial_path = initial_snapshot.path.clone();
    let refreshed_path = refreshed_snapshot.path.clone();
    assert_ne!(initial_path, refreshed_path);
    assert_eq!(initial_path.exists(), true);
    assert_eq!(refreshed_path.exists(), true);

    drop(initial_snapshot);

    assert_eq!(initial_path.exists(), false);
    assert_eq!(refreshed_path.exists(), true);

    drop(refreshed_snapshot);

    assert_eq!(refreshed_path.exists(), false);

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_shell_does_not_inherit_stdin() -> Result<()> {
    let _stdin_guard = BlockingStdinPipe::install()?;

    let dir = tempdir()?;
    let home = dir.path().abs();
    let read_status_path = home.join("stdin-read-status");
    let read_status_display = read_status_path.display();
    // Persist the startup `read` exit status so the test can assert whether
    // bash saw EOF on stdin after the snapshot process exits.
    let bashrc = format!("read -t 1 -r ignored\nprintf '%s' \"$?\" > \"{read_status_display}\"\n");
    fs::write(home.join(".bashrc"), bashrc).await?;

    let shell = Shell {
        shell_type: ShellType::Bash,
        shell_path: PathBuf::from("/bin/bash"),
    };

    let home_display = home.display();
    let script = format!(
        "HOME=\"{home_display}\"; export HOME; {}",
        snapshot_capture_script(
            ShellType::Bash,
            SnapshotCaptureOptions {
                startup: SnapshotStartup::Interactive,
                declarations: true,
                environment: false,
            }
        )
        .expect("bash supports snapshots")
    );
    let output = run_script_with_timeout(
        &shell,
        &script,
        Duration::from_secs(2),
        /*login_shell*/ true,
        &home,
    )
    .await
    .context("run snapshot command")?;
    let read_status = fs::read_to_string(&read_status_path)
        .await
        .context("read stdin probe status")?;

    assert_eq!(
        read_status, "1",
        "expected shell startup read to see EOF on stdin; status={read_status:?}"
    );

    assert!(
        output.contains("# Snapshot file"),
        "expected snapshot marker in output; output={output:?}"
    );

    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn timed_out_snapshot_shell_is_terminated() -> Result<()> {
    use std::process::Stdio;
    use tokio::time::Duration as TokioDuration;
    use tokio::time::Instant;
    use tokio::time::sleep;

    let dir = tempdir()?;
    let pid_path = dir.path().join("pid");
    let script = format!("echo $$ > \"{}\"; sleep 30", pid_path.display());

    let shell = Shell {
        shell_type: ShellType::Sh,
        shell_path: PathBuf::from("/bin/sh"),
    };

    let err = run_script_with_timeout(
        &shell,
        &script,
        Duration::from_secs(1),
        /*login_shell*/ true,
        &dir.path().abs(),
    )
    .await
    .expect_err("snapshot shell should time out");
    assert!(
        err.to_string().contains("timed out"),
        "expected timeout error, got {err:?}"
    );

    let pid = fs::read_to_string(&pid_path)
        .await
        .expect("snapshot shell writes its pid before timing out")
        .trim()
        .parse::<i32>()?;

    let deadline = Instant::now() + TokioDuration::from_secs(1);
    loop {
        let kill_status = StdCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()?;
        if !kill_status.success() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("timed out snapshot shell is still alive after grace period");
        }
        sleep(TokioDuration::from_millis(50)).await;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_zsh_snapshot_includes_sections() -> Result<()> {
    let snapshot = get_snapshot(ShellType::Zsh).await?;
    assert_posix_snapshot_sections(&snapshot);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_bash_snapshot_includes_sections() -> Result<()> {
    let snapshot = get_snapshot(ShellType::Bash).await?;
    assert_posix_snapshot_sections(&snapshot);
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_sh_snapshot_includes_sections() -> Result<()> {
    let snapshot = get_snapshot(ShellType::Sh).await?;
    assert_posix_snapshot_sections(&snapshot);
    Ok(())
}

#[cfg(target_os = "windows")]
#[ignore]
#[tokio::test]
async fn windows_powershell_snapshot_includes_sections() -> Result<()> {
    let snapshot = get_snapshot(ShellType::PowerShell).await?;
    assert!(snapshot.contains("# Snapshot file"));
    assert!(snapshot.contains("aliases "));
    assert!(snapshot.contains("exports "));
    Ok(())
}

async fn write_rollout_stub(codex_home: &Path, session_id: ThreadId) -> Result<PathBuf> {
    let dir = codex_home
        .join("sessions")
        .join("2025")
        .join("01")
        .join("01");
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("rollout-2025-01-01T00-00-00-{session_id}.jsonl"));
    fs::write(&path, "").await?;
    Ok(path)
}

#[tokio::test]
async fn cleanup_stale_snapshots_removes_orphans_and_keeps_live() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let live_session = ThreadId::new();
    let orphan_session = ThreadId::new();
    let live_snapshot = snapshot_dir.join(format!("{live_session}.123.sh"));
    let orphan_snapshot = snapshot_dir.join(format!("{orphan_session}.456.sh"));
    let invalid_snapshot = snapshot_dir.join("not-a-snapshot.txt");

    write_rollout_stub(&codex_home, live_session).await?;
    fs::write(&live_snapshot, "live").await?;
    fs::write(&orphan_snapshot, "orphan").await?;
    fs::write(&invalid_snapshot, "invalid").await?;

    cleanup_stale_snapshots(&codex_home, ThreadId::new(), /*state_db*/ None).await?;

    assert_eq!(live_snapshot.exists(), true);
    assert_eq!(orphan_snapshot.exists(), false);
    assert_eq!(invalid_snapshot.exists(), false);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_stale_snapshots_removes_stale_rollouts() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let stale_session = ThreadId::new();
    let stale_snapshot = snapshot_dir.join(format!("{stale_session}.123.sh"));
    let rollout_path = write_rollout_stub(&codex_home, stale_session).await?;
    fs::write(&stale_snapshot, "stale").await?;

    set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

    cleanup_stale_snapshots(&codex_home, ThreadId::new(), /*state_db*/ None).await?;

    assert_eq!(stale_snapshot.exists(), false);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_stale_snapshots_skips_active_session() -> Result<()> {
    let dir = tempdir()?;
    let codex_home = dir.path().abs();
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
    fs::create_dir_all(&snapshot_dir).await?;

    let active_session = ThreadId::new();
    let active_snapshot = snapshot_dir.join(format!("{active_session}.123.sh"));
    let rollout_path = write_rollout_stub(&codex_home, active_session).await?;
    fs::write(&active_snapshot, "active").await?;

    set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

    cleanup_stale_snapshots(&codex_home, active_session, /*state_db*/ None).await?;

    assert_eq!(active_snapshot.exists(), true);
    Ok(())
}

#[cfg(unix)]
fn set_file_mtime(path: &Path, age: Duration) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs()
        .saturating_sub(age.as_secs());
    let tv_sec = now
        .try_into()
        .map_err(|_| anyhow!("Snapshot mtime is out of range for libc::timespec"))?;
    let ts = libc::timespec { tv_sec, tv_nsec: 0 };
    let times = [ts, ts];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_rejects_nul_script_before_execution() -> Result<()> {
    let root = tempdir()?;
    let shell = Shell {
        shell_type: ShellType::Sh,
        shell_path: PathBuf::from("/bin/sh"),
    };
    let error = run_script_with_timeout(
        &shell,
        "printf ran > ran\0",
        Duration::from_secs(5),
        /*login_shell*/ false,
        &root.path().abs(),
    )
    .await
    .expect_err("invalid snapshot script must fail to spawn");
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::InvalidInput)
    );
    assert!(!root.path().join("ran").exists());
    Ok(())
}

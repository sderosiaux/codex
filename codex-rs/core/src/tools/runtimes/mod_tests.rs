use super::*;
use crate::shell::ShellType;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::PathBufExt;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::process::Command;
use tempfile::tempdir;

fn shell_with_snapshot(
    shell_type: ShellType,
    shell_path: &str,
    snapshot_path: AbsolutePathBuf,
) -> (Shell, AbsolutePathBuf) {
    (
        Shell {
            shell_type,
            shell_path: PathBuf::from(shell_path),
        },
        snapshot_path,
    )
}

#[cfg(unix)]
#[test]
fn runtime_path_prepends_records_runtime_path_prepend() {
    let mut env = HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let mut runtime_path_prepends = RuntimePathPrepends::default();

    runtime_path_prepends.prepend(&mut env, PathBuf::from("/package/codex-path").as_path());

    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/package/codex-path:/usr/bin:/bin"),
        "runtime PATH prepend should update the live exec environment"
    );
    assert_eq!(
        runtime_path_prepends.entries,
        vec!["/package/codex-path"],
        "runtime PATH prepend should be recorded for snapshot replay"
    );
}

#[cfg(unix)]
#[test]
fn runtime_path_prepends_drops_empty_path_entries() {
    let mut env = HashMap::from([(
        "PATH".to_string(),
        ":/usr/bin:/package/codex-path::/bin:".to_string(),
    )]);
    let mut runtime_path_prepends = RuntimePathPrepends::default();

    runtime_path_prepends.prepend(&mut env, PathBuf::from("/package/codex-path").as_path());

    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/package/codex-path:/usr/bin:/bin"),
        "empty PATH entries should be dropped instead of preserving current-directory lookup"
    );
    assert_eq!(
        runtime_path_prepends.entries,
        vec!["/package/codex-path"],
        "deduped runtime PATH prepend should still be recorded once"
    );
}

#[cfg(unix)]
#[test]
fn runtime_path_prepends_ignores_empty_path_entry() {
    let mut env = HashMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    let mut runtime_path_prepends = RuntimePathPrepends::default();

    runtime_path_prepends.prepend(&mut env, PathBuf::new().as_path());

    assert_eq!(
        env.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin"),
        "empty runtime PATH prepend should leave PATH unchanged"
    );
    assert_eq!(
        runtime_path_prepends,
        RuntimePathPrepends::default(),
        "empty runtime PATH prepend should not be recorded for snapshot replay"
    );
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_bootstraps_in_user_shell() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, "# Snapshot file\n").expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Zsh, "/bin/zsh", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "echo hello".to_string(),
    ];

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    assert_eq!(rewritten[0], "/bin/zsh");
    assert_eq!(rewritten[1], "-c");
    assert!(rewritten[2].contains("if . '"));
    assert!(rewritten[2].contains("exec '/bin/bash' -c 'echo hello'"));
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_escapes_single_quotes() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, "# Snapshot file\n").expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Zsh, "/bin/zsh", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "echo 'hello'".to_string(),
    ];

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    assert!(rewritten[2].contains(r#"exec '/bin/bash' -c 'echo '"'"'hello'"'"''"#));
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_uses_bash_bootstrap_shell() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, "# Snapshot file\n").expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/zsh".to_string(),
        "-lc".to_string(),
        "echo hello".to_string(),
    ];

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    assert_eq!(rewritten[0], "/bin/bash");
    assert_eq!(rewritten[1], "-c");
    assert!(rewritten[2].contains("if . '"));
    assert!(rewritten[2].contains("exec '/bin/zsh' -c 'echo hello'"));
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_uses_sh_bootstrap_shell() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, "# Snapshot file\n").expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Sh, "/bin/sh", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "echo hello".to_string(),
    ];

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    assert_eq!(rewritten[0], "/bin/sh");
    assert_eq!(rewritten[1], "-c");
    assert!(rewritten[2].contains("if . '"));
    assert!(rewritten[2].contains("exec '/bin/bash' -c 'echo hello'"));
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_preserves_trailing_args() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, "# Snapshot file\n").expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Zsh, "/bin/zsh", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s %s' \"$0\" \"$1\"".to_string(),
        "arg0".to_string(),
        "arg1".to_string(),
    ];

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    assert!(
        rewritten[2]
            .contains(r#"exec '/bin/bash' -c 'printf '"'"'%s %s'"'"' "$0" "$1"' 'arg0' 'arg1'"#)
    );
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_restores_explicit_override_precedence() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport TEST_ENV_SNAPSHOT=global\nexport SNAPSHOT_ONLY=from_snapshot\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s|%s' \"$TEST_ENV_SNAPSHOT\" \"${SNAPSHOT_ONLY-unset}\"".to_string(),
    ];
    let explicit_env_overrides =
        HashMap::from([("TEST_ENV_SNAPSHOT".to_string(), "worktree".to_string())]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &explicit_env_overrides,
        &HashMap::from([("TEST_ENV_SNAPSHOT".to_string(), "worktree".to_string())]),
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env("TEST_ENV_SNAPSHOT", "worktree")
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "worktree|from_snapshot"
    );
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_restores_codex_metadata_from_env() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport CODEX_THREAD_ID='parent-thread'\nexport CODEX_TOOL_CALL_ID='parent-call'\nexport CODEX_VERSION='old-version'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s|%s|%s' \"$CODEX_THREAD_ID\" \"$CODEX_TOOL_CALL_ID\" \"$CODEX_VERSION\""
            .to_string(),
    ];
    let env = HashMap::from([
        ("CODEX_THREAD_ID".to_string(), "nested-thread".to_string()),
        ("CODEX_TOOL_CALL_ID".to_string(), "nested-call".to_string()),
        (
            "CODEX_VERSION".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
    ]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &env,
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .envs(env)
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!("nested-thread|nested-call|", env!("CARGO_PKG_VERSION")),
    );
}

#[test_case::test_case(CODEX_TOOL_CALL_ID_ENV_VAR; "tool call ID")]
fn maybe_wrap_shell_lc_with_snapshot_unsets_absent_metadata(key: &str) -> anyhow::Result<()> {
    let dir = tempdir()?;
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        format!("# Snapshot file\nexport {key}='stale-value'\n"),
    )?;
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        format!("printenv {key}"),
    ];
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env_remove(key)
        .output()?;

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
    Ok(())
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_restores_apply_patch_rollout_state() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS='stale'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printenv CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS".to_string(),
    ];
    let env = HashMap::from([(
        CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR.to_string(),
        "1".to_string(),
    )]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &env,
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env(CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR, "1")
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n");

    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env_remove(CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR)
        .output()
        .expect("run rewritten command");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"");
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_keeps_snapshot_path_without_override() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport PATH='/snapshot/bin'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s' \"$PATH\"".to_string(),
    ];
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &HashMap::new(),
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "/snapshot/bin");
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_applies_explicit_path_override() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport PATH='/snapshot/bin'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s' \"$PATH\"".to_string(),
    ];
    let explicit_env_overrides = HashMap::from([("PATH".to_string(), "/worktree/bin".to_string())]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &explicit_env_overrides,
        &HashMap::from([("PATH".to_string(), "/worktree/bin".to_string())]),
        &RuntimePathPrepends::default(),
    );
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env("PATH", "/worktree/bin")
        .output()
        .expect("run rewritten command");

    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "/worktree/bin");
}

#[cfg(unix)]
#[test]
fn maybe_wrap_shell_lc_with_snapshot_preserves_package_path_prepend() -> anyhow::Result<()> {
    let (stdout, package_path_dir) =
        run_snapshot_path_probe_with_runtime_path_prepend(HashMap::new())?;

    assert_eq!(
        stdout,
        format!("{}:/snapshot/bin", package_path_dir.display()),
        "package path prepend should replay ahead of snapshot PATH"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn maybe_wrap_shell_lc_with_snapshot_applies_runtime_path_prepend_after_explicit_path_override()
-> anyhow::Result<()> {
    let (stdout, package_path_dir) = run_snapshot_path_probe_with_runtime_path_prepend(
        HashMap::from([("PATH".to_string(), "/worktree/bin".to_string())]),
    )?;

    assert_eq!(
        stdout,
        format!("{}:/worktree/bin", package_path_dir.display()),
        "explicit PATH override should suppress snapshot PATH while preserving runtime prepend"
    );
    Ok(())
}

#[cfg(unix)]
fn run_snapshot_path_probe_with_runtime_path_prepend(
    explicit_env_overrides: HashMap<String, String>,
) -> anyhow::Result<(String, PathBuf)> {
    let dir = tempdir()?;
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport PATH='/snapshot/bin'\n",
    )?;
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s' \"$PATH\"".to_string(),
    ];
    let package_path_dir = dir.path().join("codex-path");
    let mut env = HashMap::from([("PATH".to_string(), "/worktree/bin".to_string())]);
    let mut runtime_path_prepends = RuntimePathPrepends::default();
    runtime_path_prepends.prepend(&mut env, package_path_dir.as_path());
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &explicit_env_overrides,
        &env,
        &runtime_path_prepends,
    );
    let path = env
        .get("PATH")
        .ok_or_else(|| anyhow::anyhow!("PATH should be set"))?;
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env("PATH", path)
        .output()?;

    assert!(output.status.success(), "command failed: {output:?}");
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        package_path_dir,
    ))
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_does_not_embed_override_values_in_argv() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport OPENAI_API_KEY='snapshot-value'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
        "/bin/bash".to_string(),
        "-lc".to_string(),
        "printf '%s' \"$OPENAI_API_KEY\"".to_string(),
    ];
    let explicit_env_overrides = HashMap::from([
        (
            "OPENAI_API_KEY".to_string(),
            "super-secret-value".to_string(),
        ),
        (
            "openai_identity_token_file".to_string(),
            "/run/identity-token".to_string(),
        ),
    ]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &explicit_env_overrides,
        &HashMap::from([(
            "OPENAI_API_KEY".to_string(),
            "super-secret-value".to_string(),
        )]),
        &RuntimePathPrepends::default(),
    );

    assert!(!rewritten[2].contains("super-secret-value"));
    assert!(!rewritten[2].contains("openai_identity_token_file"));
    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env("OPENAI_API_KEY", "super-secret-value")
        .output()
        .expect("run rewritten command");
    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "super-secret-value"
    );
}

#[test]
fn maybe_wrap_shell_lc_with_snapshot_preserves_unset_override_variables() {
    let dir = tempdir().expect("create temp dir");
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(
        &snapshot_path,
        "# Snapshot file\nexport CODEX_TEST_UNSET_OVERRIDE='snapshot-value'\n",
    )
    .expect("write snapshot");
    let (session_shell, shell_snapshot) =
        shell_with_snapshot(ShellType::Bash, "/bin/bash", snapshot_path.abs());
    let command = vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            "if [ \"${CODEX_TEST_UNSET_OVERRIDE+x}\" = x ]; then printf 'set:%s' \"$CODEX_TEST_UNSET_OVERRIDE\"; else printf 'unset'; fi".to_string(),
        ];
    let explicit_env_overrides = HashMap::from([(
        "CODEX_TEST_UNSET_OVERRIDE".to_string(),
        "worktree-value".to_string(),
    )]);
    let rewritten = maybe_wrap_shell_lc_with_snapshot(
        &command,
        &session_shell,
        Some(&shell_snapshot),
        &explicit_env_overrides,
        &HashMap::new(),
        &RuntimePathPrepends::default(),
    );

    let output = Command::new(&rewritten[0])
        .args(&rewritten[1..])
        .env_remove("CODEX_TEST_UNSET_OVERRIDE")
        .output()
        .expect("run rewritten command");
    assert!(output.status.success(), "command failed: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");
}

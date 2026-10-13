use super::CapturedSnapshot;
use super::SnapshotCaptureOptions;
use super::SnapshotStartup;
use super::snapshot_capture_script;
use crate::shell_detect::ShellType;
use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use std::process::Command;
use tempfile::tempdir;

const CAPTURE_ALL: SnapshotCaptureOptions = SnapshotCaptureOptions {
    startup: SnapshotStartup::Interactive,
    declarations: true,
    environment: true,
};

const CAPTURE_NON_INTERACTIVE: SnapshotCaptureOptions = SnapshotCaptureOptions {
    startup: SnapshotStartup::NonInteractive,
    ..CAPTURE_ALL
};

fn snapshot_script(shell_type: ShellType) -> Option<String> {
    snapshot_capture_script(shell_type, CAPTURE_ALL)
}

#[test]
fn snapshot_capture_requires_marker_and_complete_records() {
    for captured in [
        "missing header\0\0\0",
        "# Snapshot file\n\0",
        "# Snapshot file\n\0\0NAME\0export NAME=value\n",
    ] {
        assert!(CapturedSnapshot::parse(ShellType::Bash, captured.as_bytes()).is_none());
    }
}

#[test]
fn snapshot_capture_selects_declarations_and_environment() -> Result<()> {
    let dir = tempdir()?;
    for (shell, shell_type) in [
        ("/bin/bash", ShellType::Bash),
        ("/bin/sh", ShellType::Sh),
        ("/bin/zsh", ShellType::Zsh),
    ] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        for (declarations, environment) in
            [(true, false), (false, true), (true, true), (false, false)]
        {
            let script = snapshot_capture_script(
                shell_type,
                SnapshotCaptureOptions {
                    startup: SnapshotStartup::NonInteractive,
                    declarations,
                    environment,
                },
            )
            .unwrap();
            let output = Command::new(shell)
                .args(["-c", &format!("readonly CODEX_THREAD_ID; {script}")])
                .env_clear()
                .env("HOME", dir.path())
                .env("PATH", "/usr/bin:/bin")
                .env("CODEX_THREAD_ID", "capture-thread")
                .env("CODEX_TOOL_CALL_ID", "profile-call")
                .env("APP_SETTING", "value")
                .output()?;
            assert!(output.status.success(), "{shell}: {output:?}");
            let captured = CapturedSnapshot::parse(shell_type, &output.stdout).unwrap();
            assert_eq!(
                (
                    captured
                        .exports
                        .iter()
                        .any(|export| export.key == "APP_SETTING"),
                    captured
                        .environment
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == b"APP_SETTING=value"),
                ),
                (declarations, environment),
                "{shell}"
            );
            if !declarations || !environment {
                continue;
            }

            // Replay must take execution metadata from the current launch, not the capture.
            let snapshot = captured
                .render_script(&codex_protocol::config_types::ShellEnvironmentPolicy::default());
            assert!(!snapshot.contains("CODEX_THREAD_ID"), "{shell}: {snapshot}");
            assert!(
                !snapshot.contains("CODEX_TOOL_CALL_ID"),
                "{shell}: {snapshot}"
            );
            let path = dir.path().join("snapshot.sh");
            std::fs::write(&path, snapshot)?;
            let replay = Command::new(shell)
                .args(["-c", "set -e; . \"$1\"; printf '%s|%s|%s' \"$CODEX_THREAD_ID\" \"$CODEX_TOOL_CALL_ID\" \"$APP_SETTING\"", "replay"])
                .arg(&path)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("CODEX_THREAD_ID", "current-thread")
                .env("CODEX_TOOL_CALL_ID", "current-call")
                .output()?;
            assert!(replay.status.success(), "{shell}: {replay:?}");
            assert_eq!(
                replay.stdout, b"current-thread|current-call|value",
                "{shell}"
            );
        }
    }
    Ok(())
}

#[test]
fn bash_snapshot_filters_invalid_exports() -> Result<()> {
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(snapshot_script(ShellType::Bash).expect("bash supports snapshots"))
        .env("BASH_ENV", "/dev/null")
        .env("VALID_NAME", "ok")
        .env("PWD", "/tmp/stale")
        .env("NEXTEST_BIN_EXE_codex-write-config-schema", "/path/to/bin")
        .env("BAD-NAME", "broken")
        .output()?;

    assert!(output.status.success());

    let stdout = captured_script(ShellType::Bash, &String::from_utf8(output.stdout)?)?;
    assert!(stdout.contains("VALID_NAME"));
    assert!(!stdout.contains("PWD=/tmp/stale"));
    assert!(!stdout.contains("NEXTEST_BIN_EXE_codex-write-config-schema"));
    assert!(!stdout.contains("BAD-NAME"));

    Ok(())
}

#[test]
fn bash_snapshot_preserves_multiline_exports() -> Result<()> {
    let multiline_cert = "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----";
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(snapshot_script(ShellType::Bash).expect("bash supports snapshots"))
        .env("BASH_ENV", "/dev/null")
        .env("MULTILINE_CERT", multiline_cert)
        .output()?;

    assert!(output.status.success());

    let stdout = captured_script(ShellType::Bash, &String::from_utf8(output.stdout)?)?;
    assert!(
        stdout.contains("MULTILINE_CERT=") || stdout.contains("MULTILINE_CERT"),
        "snapshot should include the multiline export name"
    );

    let dir = tempdir()?;
    let snapshot_path = dir.path().join("snapshot.sh");
    std::fs::write(&snapshot_path, stdout.as_bytes())?;

    let validate = Command::new("/bin/bash")
        .arg("-c")
        .arg("set -e; . \"$1\"")
        .arg("bash")
        .arg(&snapshot_path)
        .env("BASH_ENV", "/dev/null")
        .output()?;

    assert!(
        validate.status.success(),
        "snapshot validation failed: {}",
        String::from_utf8_lossy(&validate.stderr)
    );

    Ok(())
}

#[test]
fn snapshot_environment_preserves_native_path_exports_and_shell_level() -> Result<()> {
    let dir = tempdir()?;
    for (shell, shell_type, setup) in [
        ("/bin/bash", ShellType::Bash, "export -n PATH"),
        ("/bin/zsh", ShellType::Zsh, "typeset +x PATH"),
    ] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        let run = |script: &str| {
            // Keep Bash's final-command exec optimization from decrementing SHLVL.
            Command::new(shell)
                .args(["-c", &format!("{setup}\n{script}\n:")])
                .env_clear()
                .env("HOME", dir.path())
                .env("PATH", "/usr/bin:/bin")
                .env("SHLVL", "3")
                .output()
        };
        let native = run("/usr/bin/env -0")?;
        let script = snapshot_capture_script(
            shell_type,
            SnapshotCaptureOptions {
                startup: SnapshotStartup::NonInteractive,
                ..CAPTURE_ALL
            },
        )
        .unwrap();
        let output = run(&script)?;
        assert!(native.status.success() && output.status.success());
        let captured = CapturedSnapshot::parse(shell_type, &output.stdout).unwrap();
        let metadata = |environment: &[u8]| {
            environment
                .split(|byte| *byte == 0)
                .filter(|entry| entry.starts_with(b"PATH=") || entry.starts_with(b"SHLVL="))
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            metadata(captured.environment),
            metadata(&native.stdout),
            "{shell}"
        );
    }
    Ok(())
}

#[test]
fn sh_declarations_preserve_path_export_state() -> Result<()> {
    let dir = tempdir()?;
    let snapshot_path = dir.path().join("snapshot.sh");
    let script = snapshot_capture_script(
        ShellType::Sh,
        SnapshotCaptureOptions {
            startup: SnapshotStartup::NonInteractive,
            ..CAPTURE_ALL
        },
    )
    .expect("sh supports snapshots");
    for shell in [
        "/bin/sh",
        #[cfg(target_os = "macos")]
        "/bin/dash",
    ] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        for (setup, expected) in [
            ("PATH=/enterprise/bin; export PATH", "x:/enterprise/bin"),
            ("PATH=; export PATH", "x:"),
            ("unset PATH; export PATH", ":"),
            ("unset PATH; PATH=/enterprise/bin", ":"),
        ] {
            let captured = Command::new(shell)
                .args(["-uc", &format!("{setup}\n{script}")])
                .env_clear()
                .output()?;
            assert!(captured.status.success(), "capture failed: {captured:?}");
            std::fs::write(
                &snapshot_path,
                captured_script(ShellType::Sh, &String::from_utf8(captured.stdout)?)?,
            )?;
            let restored = Command::new(shell)
                .args([
                    "-c",
                    "unset PATH; . \"$1\"; printf '%s:%s' \"${PATH+x}\" \"${PATH-}\"",
                    "snapshot",
                ])
                .arg(&snapshot_path)
                .env_clear()
                .output()?;
            assert!(restored.status.success(), "replay failed: {restored:?}");
            assert_eq!(restored.stdout, expected.as_bytes(), "{setup}");
        }
    }
    Ok(())
}

#[test]
fn posix_startup_path_expansion_preserves_supported_forms() -> Result<()> {
    let script = format!(
        "{}\n__codex_snapshot_expand_env \"$ENV\"",
        super::posix_env_path_expansion_function()
    );
    let output = Command::new("/bin/sh")
        .arg("-uc")
        .arg(&script)
        .env("ENV", "$MISSING")
        .output()?;

    assert!(output.status.success(), "snapshot failed: {output:?}");
    assert_eq!(output.stdout, b"$MISSING");

    let dir = tempdir()?;
    let startup = dir.path().join("startup.sh");
    for (env_value, expansion_env) in [
        (
            startup.display().to_string(),
            Vec::<(String, String)>::new(),
        ),
        (
            "$STARTUP_FILE".to_string(),
            vec![("STARTUP_FILE".to_string(), startup.display().to_string())],
        ),
        (
            "${STARTUP_FILE}".to_string(),
            vec![("STARTUP_FILE".to_string(), startup.display().to_string())],
        ),
        (
            "${STARTUP_DIR}/startup.sh".to_string(),
            vec![("STARTUP_DIR".to_string(), dir.path().display().to_string())],
        ),
        (
            "${PATH%%:*}/startup.sh".to_string(),
            vec![(
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", dir.path().display()),
            )],
        ),
        (
            "$PATH/startup.sh".to_string(),
            vec![("PATH".to_string(), dir.path().display().to_string())],
        ),
        (
            "${PATH}/startup.sh".to_string(),
            vec![("PATH".to_string(), dir.path().display().to_string())],
        ),
        (
            format!("$PATH{}", startup.display()),
            vec![("PATH".to_string(), String::new())],
        ),
        (
            format!("${{PATH}}{}", startup.display()),
            vec![("PATH".to_string(), String::new())],
        ),
        (
            "~/startup.sh".to_string(),
            vec![("HOME".to_string(), dir.path().display().to_string())],
        ),
    ] {
        let output = Command::new("/bin/sh")
            .arg("-uc")
            .arg(&script)
            .env("ENV", &env_value)
            .envs(expansion_env)
            .output()?;

        assert!(output.status.success(), "snapshot failed: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout)?,
            startup.display().to_string(),
            "ENV={env_value}"
        );
    }

    let default_startup_dir = dir.path().join(".config");
    for xdg_config_home in [
        None,
        Some(std::ffi::OsStr::new("")),
        Some(default_startup_dir.as_os_str()),
    ] {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-uc")
            .arg(&script)
            .env("HOME", dir.path())
            .env("ENV", "${XDG_CONFIG_HOME:-$HOME/.config}/startup.sh");
        if let Some(xdg_config_home) = xdg_config_home {
            command.env("XDG_CONFIG_HOME", xdg_config_home);
        } else {
            command.env_remove("XDG_CONFIG_HOME");
        }
        let output = command.output()?;

        assert!(output.status.success(), "snapshot failed: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout)?,
            default_startup_dir.join("startup.sh").display().to_string()
        );
    }

    let injected_marker = dir.path().join("env-injected");
    let injected_env = format!(
        "\"; touch '{}'; __codex_env_file=\"",
        injected_marker.display()
    );
    let output = Command::new("/bin/sh")
        .arg("-uc")
        .arg(&script)
        .env("ENV", injected_env)
        .output()?;
    assert!(output.status.success(), "snapshot failed: {output:?}");
    assert!(
        !injected_marker.exists(),
        "snapshot interpreted ENV as shell source"
    );

    Ok(())
}

#[test]
fn zsh_snapshot_preserves_deferred_quote_modes() -> Result<()> {
    if !std::path::Path::new("/bin/zsh").exists() {
        return Ok(());
    }
    let dir = tempdir()?;
    let script = snapshot_capture_script(ShellType::Zsh, CAPTURE_NON_INTERACTIVE)
        .expect("zsh supports snapshots");
    for (real, startup) in [
        (
            "token_ab'cdefghijklmnopqrstuvwx",
            "helper() { printf '%s' \"$(printf '%s' 'token_ab''cdefghijklmnopqrstuvwx')\"; }\nsetopt rcquotes\n",
        ),
        (
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "alias helper=\"setopt rcquotes; printf '%s' 'ghp_abcdefghijklmno''pqrstuvwxyz0123456789'\"\n",
        ),
        (
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "alias helper=\"printf '%s' 'ghp_abcdefghijkl'\\\\\n'mnopqrstuvwxyz0123456789'\"\nsetopt rcquotes\n",
        ),
    ] {
        let startup_path = dir.path().join("startup.sh");
        std::fs::write(&startup_path, startup)?;
        let output = Command::new("/bin/zsh")
            .args(["-fc", &format!(". \"$1\"\n{script}"), "snapshot"])
            .arg(startup_path)
            .env_clear()
            .env("HOME", dir.path())
            .output()?;
        assert!(output.status.success(), "{output:?}");
        let snapshot = captured_script(ShellType::Zsh, &String::from_utf8(output.stdout)?)?;
        let path = dir.path().join("snapshot.sh");
        std::fs::write(&path, &snapshot)?;
        let replay = Command::new("/bin/zsh")
            .args(["-fc", ". \"$1\"; eval helper", "snapshot"])
            .arg(path)
            .env_clear()
            .output()?;
        assert!(replay.status.success(), "{replay:?}");
        assert_eq!(String::from_utf8(replay.stdout)?, real);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn zsh_snapshot_restores_tied_path() -> Result<()> {
    let dir = tempdir()?;
    let path_with_spaces = dir.path().join("path with spaces").join("bin");
    let plain_path = dir.path().join("plain-path").join("bin");
    let expected_path = format!(
        "{}:{}:/usr/bin:/bin",
        path_with_spaces.display(),
        plain_path.display()
    );
    let zshrc = format!(
        "export -UT PATH path=('{}' '{}' '{}' /usr/bin /bin)\n",
        path_with_spaces.display(),
        plain_path.display(),
        plain_path.display()
    );
    std::fs::write(dir.path().join(".zshrc"), zshrc)?;

    let snapshot = Command::new("/bin/zsh")
        .arg("-f")
        .arg("-c")
        .arg(snapshot_script(ShellType::Zsh).expect("zsh supports snapshots"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("ZDOTDIR", dir.path())
        .output()?;
    assert!(snapshot.status.success());

    let snapshot_path = dir.path().join("snapshot.sh");
    let snapshot = captured_script(ShellType::Zsh, &String::from_utf8(snapshot.stdout)?)?;
    std::fs::write(&snapshot_path, &snapshot)?;

    let restored = Command::new("/bin/zsh")
        .arg("-f")
        .arg("-c")
        .arg("set -e; . \"$1\"; print -r -- \"$PATH\"")
        .arg("zsh")
        .arg(&snapshot_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    assert!(restored.status.success());
    assert_eq!(
        String::from_utf8(restored.stdout)?.trim_end(),
        expected_path
    );

    assert!(
        snapshot
            .lines()
            .any(|line| line.starts_with("export -UT PATH path=")),
        "snapshot should capture the tied PATH export"
    );

    std::fs::write(dir.path().join(".zshrc"), "readonly PATH\n")?;
    let readonly_snapshot = Command::new("/bin/zsh")
        .arg("-f")
        .arg("-c")
        .arg(snapshot_script(ShellType::Zsh).expect("zsh supports snapshots"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("ZDOTDIR", dir.path())
        .output()?;
    assert!(readonly_snapshot.status.success());
    let readonly_snapshot = captured_script(
        ShellType::Zsh,
        &String::from_utf8(readonly_snapshot.stdout)?,
    )?;
    std::fs::write(&snapshot_path, &readonly_snapshot)?;

    let readonly_restored = Command::new("/bin/zsh")
        .arg("-f")
        .arg("-c")
        .arg("set -e; . \"$1\"; export PATH='/codex-path':\"$PATH\"; print -r -- \"$PATH\"")
        .arg("zsh")
        .arg(&snapshot_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    assert!(readonly_restored.status.success());
    assert_eq!(
        String::from_utf8(readonly_restored.stdout)?.trim_end(),
        "/codex-path:/usr/bin:/bin"
    );

    assert!(
        !readonly_snapshot
            .lines()
            .any(|line| line.starts_with("export -rT PATH path=")),
        "snapshot should not capture the readonly tied PATH export"
    );

    Ok(())
}

// Literal-only fixtures are shell source rather than capture output. Give those fixtures
// empty alias/export sections; real shell fixtures exercise the full framed capture.
fn captured_script(shell_type: ShellType, source: &str) -> Result<String> {
    let captured =
        CapturedSnapshot::parse(shell_type, source.as_bytes()).context("invalid native capture")?;
    Ok(captured.render_script(&codex_protocol::config_types::ShellEnvironmentPolicy::default()))
}

#[test]
fn streamed_snapshot_preserves_alias_and_option_parse_order() -> Result<()> {
    for (shell, shell_type, options, expected) in [
        (
            "/bin/bash",
            ShellType::Bash,
            "shopt -s expand_aliases",
            "onetwohelper",
        ),
        (
            "/bin/zsh",
            ShellType::Zsh,
            "module_path=()\nsetopt RC_QUOTES",
            "one'twohelper",
        ),
        (
            "/bin/zsh",
            ShellType::Zsh,
            "module_path=()\nsetopt NO_RC_QUOTES",
            "onetwohelper",
        ),
    ] {
        if !std::path::Path::new(shell).exists() {
            continue;
        }
        let dir = tempdir()?;
        let setup = format!(
            "{options}\nhelper() {{ printf helper; }}\nfunction : () {{ exit 41; }}\nalias snapshot_alias=\"printf 'one''two'\"\n"
        );
        let mut results = Vec::new();
        // Cover V1's native source, V2's source, and V2's environment fallback.
        for (source_capture, source_replay) in [(false, true), (true, true), (true, false)] {
            let capture = if source_capture {
                super::snapshot_source_capture_script(shell_type, CAPTURE_NON_INTERACTIVE)
            } else {
                snapshot_capture_script(shell_type, CAPTURE_NON_INTERACTIVE)
            }
            .unwrap();
            let shadow = if source_capture && shell_type == ShellType::Zsh {
                "alias alias=false\n"
            } else {
                ""
            };
            let output = Command::new(shell)
                .args(["-c", &format!("{setup}{shadow}{capture}")])
                .output()?;
            assert!(output.status.success(), "{shell}: {output:?}");
            let captured = CapturedSnapshot::parse(shell_type, &output.stdout).unwrap();
            let path = dir.path().join("state");
            std::fs::write(&path, captured.render_state())?;
            let restore = if source_replay {
                ". \"$1\""
            } else {
                "eval \"$(cat \"$1\")\""
            };
            let command =
                format!("{restore}\n\\alias snapshot_alias\neval snapshot_alias\nhelper\n");
            let output = Command::new(shell)
                .args(["-c", &command, "snapshot-test"])
                .arg(&path)
                .output()?;
            assert!(output.status.success(), "{shell}: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("snapshot_alias="));
            assert!(String::from_utf8_lossy(&output.stdout).ends_with(expected));
            results.push((output.stdout, output.stderr));
        }
        assert_eq!(results[0], results[1], "{shell}");
        assert_eq!(results[0], results[2], "{shell}");
    }
    Ok(())
}

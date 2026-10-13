use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

pub(super) struct StartupProcesses(pub(super) std::path::PathBuf);

impl Drop for StartupProcesses {
    fn drop(&mut self) {
        if let Ok(contents) = std::fs::read_to_string(&self.0) {
            for pid in contents
                .split_whitespace()
                .filter_map(|pid| pid.parse::<i32>().ok())
            {
                if pid > 0 {
                    // SAFETY: these PIDs were written by our startup profile.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
            }
        }
    }
}

#[test_case("bash"; "bash")]
#[cfg_attr(target_os = "macos", test_case("zsh"; "zsh"))]
#[tokio::test]
async fn shell_snapshot_preserves_successful_startup_output_and_services(
    shell: &str,
) -> Result<()> {
    let context = create_process_context(/*use_remote*/ false).await?;
    let home = TempDir::new()?;
    let _service = StartupProcesses(home.path().join("service-pid"));
    std::fs::write(
        home.path().join(format!(".{shell}rc")),
        "printf x >> \"$HOME/captures\"\n/bin/sleep 30 >/dev/null 2>&1 &\nexport SNAPSHOT_SERVICE_PID=$!\nprintf '%s' \"$SNAPSHOT_SERVICE_PID\" > \"$HOME/service-pid\"\nexec > >(/bin/sleep 0.2; /bin/cat)\nprofile_helper() { printf 'captured:%s' \"$1\"; }\n",
    )?;
    for index in 0..2 {
        let started = context
            .backend
            .start(ExecParams {
                metadata: Default::default(),
                process_id: format!("startup-capture-{index}").into(),
                argv: vec![
                    format!("/bin/{shell}"),
                    "-lc".to_string(),
                    format!(
                        "[ \"$SNAPSHOT_SERVICE_PID\" = \"$(/bin/cat \"$HOME/service-pid\")\" ] || exit 41\ncase \"$(/bin/ps -o stat= -p \"$SNAPSHOT_SERVICE_PID\")\" in ''|*Z*) exit 42;; esac\nprofile_helper {index}"
                    ),
                ],
                cwd: PathUri::from_host_native_path(home.path())?,
                env: HashMap::from([
                    (
                        "HOME".to_string(),
                        home.path().to_string_lossy().into_owned(),
                    ),
                    ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ]),
                env_policy: None,
                shell_snapshot: Some(ShellSnapshotRequest {
                    scope_id: "startup-capture".to_string(),
                    shell: ShellInfo {
                        name: shell.to_string(),
                        path: format!("/bin/{shell}"),
                    },
                }),
                tty: false,
                pipe_stdin: false,
                arg0: Some("custom-arg0".to_string()),
            })
            .await?;
        assert_eq!(
            collect_process_output_from_events(started.process).await?,
            (format!("captured:{index}"), String::new(), Some(0), true)
        );
    }
    assert_eq!(std::fs::read_to_string(home.path().join("captures"))?, "x");
    Ok(())
}

#[test_case("bash", false; "bash")]
#[test_case("bash", true; "bash_tty")]
#[cfg_attr(target_os = "macos", test_case("zsh", false; "zsh"))]
#[cfg_attr(target_os = "macos", test_case("zsh", true; "zsh_tty"))]
#[serial_test::serial(remote_exec_server)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_snapshot_concurrent_replays_keep_independent_readers(
    shell: &str,
    tty: bool,
) -> Result<()> {
    let context = create_process_context(/*use_remote*/ false).await?;
    let home = TempDir::new()?;
    let cwd = PathUri::from_host_native_path(home.path())?;
    // Keep full-size state in a function, below the 512 KiB state + environment cap.
    let payload_len = 480 * 1024;
    let payload = "x".repeat(payload_len);
    let source = if shell == "bash" {
        "${BASH_SOURCE[0]}"
    } else {
        "${(%):-%x}"
    };
    let source_check = format!("case \"{source}\" in /dev/fd/*) ;; *) return 42 ;; esac; ");
    let aliases = if shell == "zsh" {
        "module_path=()\nsetopt RC_QUOTES\nalias snapshot_quoted=\"printf '%s|' 'one''two'\"\nalias eval='exit 44'\nalias case='exit 45'\n"
    } else {
        ""
    };
    std::fs::write(
        home.path().join(format!(".{shell}rc")),
        format!(
            "printf x >> \"$HOME/captures\"\nprofile_helper() {{ {source_check}local payload='{payload}'; [ \"${{#payload}}\" = {payload_len} ] || return 43; printf 'restored:%s' \"$1\"; }}\nexec() {{ exit 41; }}\nset -u\n{aliases}"
        ),
    )?;
    let replay_checks = if shell == "zsh" {
        "eval snapshot_quoted\n[[ -o rcquotes ]] || exit 46\n"
    } else {
        ""
    };
    let command = format!("IFS= read -r line\n{replay_checks}profile_helper \"$line\"; exit 7");
    let commands = (0..8).map(|index| {
        let backend = &context.backend;
        let cwd = cwd.clone();
        let home = home.path();
        let command = command.clone();
        async move {
            let started = backend
                .start(ExecParams {
                    metadata: Default::default(),
                    process_id: format!("parallel-{index}").into(),
                    argv: vec![format!("/bin/{shell}"), "-lc".to_string(), command],
                    cwd,
                    env: HashMap::new(),
                    env_policy: Some(ExecEnvPolicy {
                        inherit: ShellEnvironmentPolicyInherit::None,
                        ignore_default_excludes: false,
                        exclude: Vec::new(),
                        r#set: HashMap::from([
                            ("HOME".to_string(), home.to_string_lossy().into_owned()),
                            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                        ]),
                        include_only: Vec::new(),
                    }),
                    shell_snapshot: Some(ShellSnapshotRequest {
                        scope_id: "parallel".to_string(),
                        shell: ShellInfo {
                            name: shell.to_string(),
                            path: format!("/bin/{shell}"),
                        },
                    }),
                    tty,
                    pipe_stdin: true,
                    arg0: None,
                })
                .await?;
            started
                .process
                .write(format!("input-{index}\n").into_bytes())
                .await?;
            let (output, errors, status, closed) =
                collect_process_output_from_events(started.process).await?;
            assert!(
                output.ends_with(&format!("restored:input-{index}")),
                "{output:?}"
            );
            if shell == "zsh" {
                assert!(
                    output.ends_with(&format!("one'two|restored:input-{index}")),
                    "{output:?}"
                );
            }
            assert_eq!((errors, status, closed), (String::new(), Some(7), true));
            Ok::<_, anyhow::Error>(())
        }
    });
    for result in futures::future::join_all(commands).await {
        result?;
    }
    assert_eq!(std::fs::read_to_string(home.path().join("captures"))?, "x");
    Ok(())
}

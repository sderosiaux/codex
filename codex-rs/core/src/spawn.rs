use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;
use tracing::trace;

use codex_protocol::shell_environment::is_non_inheritable_env_var;

#[derive(Debug, Clone, Copy)]
pub enum StdioPolicy {
    RedirectForShellTool,
    Inherit,
}

/// Spawns a child process with exactly the args, cwd, and environment supplied.
pub(crate) struct SpawnChildRequest {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: AbsolutePathBuf,
    pub stdio_policy: StdioPolicy,
    pub env: HashMap<String, String>,
}

pub(crate) async fn spawn_child_async(request: SpawnChildRequest) -> std::io::Result<Child> {
    let SpawnChildRequest {
        program,
        args,
        cwd,
        stdio_policy,
        mut env,
    } = request;

    env.retain(|name, _| !is_non_inheritable_env_var(name));

    trace!("spawn_child_async: {program:?} {args:?} {cwd:?} {stdio_policy:?} {env:?}");

    let mut cmd = match stdio_policy {
        StdioPolicy::RedirectForShellTool => {
            Command::from(codex_utils_process::background_command(&program))
        }
        StdioPolicy::Inherit => Command::new(&program),
    };
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.env_clear();
    cmd.envs(env);

    // If this Codex process dies (including being killed via SIGKILL), we want
    // any child processes that were spawned as part of a `"shell"` tool call
    // to also be terminated.

    #[cfg(unix)]
    unsafe {
        let detach_from_tty = matches!(stdio_policy, StdioPolicy::RedirectForShellTool);
        #[cfg(target_os = "linux")]
        let parent_pid = libc::getpid();
        cmd.pre_exec(move || {
            if detach_from_tty {
                codex_utils_pty::process_group::detach_from_tty()?;
            }

            // This relies on prctl(2), so it only works on Linux.
            #[cfg(target_os = "linux")]
            {
                // This prctl call effectively requests, "deliver SIGTERM when my
                // current parent dies."
                codex_utils_pty::process_group::set_parent_death_signal(parent_pid)?;
            }
            // Close descriptors accidentally inherited by the child.
            #[cfg(target_os = "macos")]
            codex_utils_pty::pty::close_inherited_fds_except(&[]);
            Ok(())
        });
    }

    match stdio_policy {
        StdioPolicy::RedirectForShellTool => {
            // Do not create a file descriptor for stdin because otherwise some
            // commands may hang forever waiting for input. For example, ripgrep has
            // a heuristic where it may try to read from stdin as explained here:
            // https://github.com/BurntSushi/ripgrep/blob/e2362d4d5185d02fa857bf381e7bd52e66fafc73/crates/core/flags/hiargs.rs#L1101-L1103
            cmd.stdin(Stdio::null());

            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        StdioPolicy::Inherit => {
            // Inherit stdin, stdout, and stderr from the parent process.
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }

    cmd.kill_on_drop(true).spawn()
}

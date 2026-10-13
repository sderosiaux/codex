/*
Runtime: unified exec

Prepares the launch command and environment for unified exec requests (shell
snapshot replay, runtime PATH prepends) and delegates PTY startup to the
process manager.
*/
use crate::exec::ExecCapturePolicy;
use crate::exec::ExecExpiration;
use crate::exec::ExecRequest;
use crate::exec::ExecServerEnvConfig;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellInvocation;
use crate::shell::ShellType;
use crate::tools::runtime::ToolCtx;
use crate::tools::runtime::ToolError;
use crate::tools::runtimes::RuntimePathPrepends;
use crate::tools::runtimes::maybe_wrap_shell_lc_with_snapshot;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_features::Feature;
use codex_shell_command::powershell::prefix_powershell_script_with_utf8;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;

mod launch;
mod snapshot_metrics;

use launch::with_launch_failure_events;

/// Request payload used by the unified-exec runtime for one process launch.
#[derive(Clone, Debug)]
pub struct UnifiedExecRequest {
    pub command: Vec<String>,
    pub(crate) shell: ShellInvocation,
    pub hook_command: String,
    pub process_id: i32,
    pub cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub env: HashMap<String, String>,
    pub exec_server_env_config: Option<ExecServerEnvConfig>,
    pub shell_snapshot: Option<codex_exec_server::ShellSnapshotRequest>,
    pub explicit_env_overrides: HashMap<String, String>,
    pub tty: bool,
}

/// Resolves the launch command for `req` and starts it through `manager`.
pub(crate) async fn launch_unified_exec(
    manager: &UnifiedExecProcessManager,
    req: &UnifiedExecRequest,
    ctx: &ToolCtx,
) -> Result<UnifiedExecProcess, ToolError> {
    let result = prepare_and_launch(manager, req, ctx).await;
    with_launch_failure_events(result, req, ctx).await
}

async fn prepare_and_launch(
    manager: &UnifiedExecProcessManager,
    req: &UnifiedExecRequest,
    ctx: &ToolCtx,
) -> Result<UnifiedExecProcess, ToolError> {
    let base_command = &req.command;
    let requested_shell = &req.shell.shell;
    let session_shell = ctx.session.user_shell();
    let shell = req
        .turn_environment
        .shell
        .as_ref()
        .unwrap_or(session_shell.as_ref());
    let snapshot_metrics =
        snapshot_metrics::SnapshotMetrics::start(req, &ctx.step_context.turn.config);
    // TODO(anp): Make shell snapshot lookup accept PathUri.
    let native_cwd = req
        .cwd
        .to_abs_path()
        .map_err(|err| ToolError::Rejected(err.to_string()))?;
    let shell_snapshot = req
        .turn_environment
        .shell_snapshot(&native_cwd, base_command, &ctx.step_context.turn.config)
        .await;
    let snapshot_wait = snapshot_metrics
        .as_ref()
        .map(|metrics| metrics.started_at.elapsed());
    let shell_snapshot_location = shell_snapshot.as_ref().map(|snapshot| snapshot.path());
    #[allow(unused_mut)]
    let mut env = req.env.clone();
    #[cfg(unix)]
    let runtime_path_prepends = {
        let mut runtime_path_prepends = RuntimePathPrepends::default();
        crate::tools::runtimes::apply_package_path_prepend(&mut env, &mut runtime_path_prepends);
        runtime_path_prepends
    };
    #[cfg(not(unix))]
    let runtime_path_prepends = RuntimePathPrepends::default();
    // Restore the executor's PATH directories inside the shell so nested commands can
    // still find bundled tools even if login startup clears the inherited PATH.
    // `base_command` already carries the requested login mode; keep it if setup is unavailable.
    // `hook_command` is the raw script the helper needs to prepend setup inside that same shell.
    let mut command = base_command.to_vec();
    if ctx.session.enabled(Feature::LoginShellPackagePath)
        && req.shell.is_posix_login()
        && !req.explicit_env_overrides.contains_key("PATH")
        && let Some(command_with_path_prepends) = req.shell.derive_exec_args_with_path_prepends(
            &req.hook_command,
            &codex_exec_server::local_prepend_path_dirs(),
        )
    {
        command = command_with_path_prepends;
    }
    let wrapped = maybe_wrap_shell_lc_with_snapshot(
        &command,
        shell,
        shell_snapshot_location.as_ref(),
        &req.explicit_env_overrides,
        &env,
        &runtime_path_prepends,
    );
    let snapshot_used = wrapped != command;
    command = wrapped;
    if let Some(metrics) = snapshot_metrics
        && let Some(wait) = snapshot_wait
    {
        let outcome = if snapshot_used { "used" } else { "fallback" };
        metrics.record(&ctx.step_context.session_telemetry, wait, outcome);
    }
    if req.shell_snapshot.is_some() {
        let exports =
            runtime_path_prepends.shell_exports_after_snapshot(&req.explicit_env_overrides);
        if !exports.is_empty()
            && let Some(script) = command.get_mut(2)
        {
            *script = format!("{exports}\n{script}");
        }
    }
    let command = if matches!(requested_shell.shell_type, ShellType::PowerShell) {
        prefix_powershell_script_with_utf8(&command)
    } else {
        command
    };
    if command.is_empty() {
        return Err(ToolError::Rejected(
            "missing command line for PTY".to_string(),
        ));
    }
    let request = ExecRequest {
        command,
        cwd: req.cwd.clone(),
        env,
        exec_server_env_config: req.exec_server_env_config.clone(),
        exec_server_shell_snapshot: req.shell_snapshot.clone(),
        expiration: ExecExpiration::DefaultTimeout,
        capture_policy: ExecCapturePolicy::ShellTool,
    };
    let mut process = manager
        .open_session_with_prepared_exec_env(
            req.process_id,
            &request,
            Some(ctx),
            req.tty,
            req.turn_environment.environment.as_ref(),
        )
        .await
        .map_err(|err| ToolError::Rejected(err.to_string()))?;
    process._shell_snapshot = shell_snapshot;
    Ok(process)
}

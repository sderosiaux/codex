use std::collections::HashMap;
use std::sync::Arc;

use codex_exec_server::ExecParams;
use codex_exec_server::ShellInfo;
use codex_exec_server::ShellSnapshotRequest;
use codex_features::Feature;
use codex_tools::ToolName;
use codex_utils_path_uri::PathUri;
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use super::ExecCommandRequest;
use super::UnifiedExecContext;
use super::process_manager::apply_unified_exec_env;
use super::process_manager::exec_env_policy_from_shell_policy;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::exec_env::inject_apply_patch_env;
use crate::exec_env::inject_session_env;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::shell::ShellType;

impl Session {
    pub(crate) fn prewarm_shell_snapshots(
        self: &Arc<Self>,
        step_context: &StepContext,
    ) -> Option<AbortOnDropHandle<()>> {
        if !self.features().enabled(Feature::ShellSnapshotV2)
            || step_context
                .tool_router
                .registered_tool(&ToolName::plain("exec_command"))
                .is_none()
        {
            return None;
        }

        let user_shell = self.user_shell();
        let environments = step_context.environments.clone();
        let session = Arc::clone(self);
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            for environment in environments.turn_environments() {
                if !environment.shell_snapshot_v2_supported
                    || !environment.config().allow_login_shell
                {
                    continue;
                }

                let shell = environment.shell.as_ref().unwrap_or(user_shell.as_ref());
                if !matches!(
                    shell.shell_type,
                    ShellType::Bash | ShellType::Zsh | ShellType::Sh
                ) {
                    continue;
                }
                let mut env = apply_unified_exec_env(HashMap::new());
                env.insert(
                    CODEX_THREAD_ID_ENV_VAR.to_string(),
                    session.thread_id().to_string(),
                );
                inject_session_env(&mut env, session.session_id());
                inject_apply_patch_env(&mut env);
                #[cfg(unix)]
                if let Some(path) = environment.shell_environment_policy().r#set.get("PATH") {
                    env.insert("PATH".to_string(), path.clone());
                    crate::tools::runtimes::apply_package_path_prepend(
                        &mut env,
                        &mut crate::tools::runtimes::RuntimePathPrepends::default(),
                    );
                }
                let params = ExecParams {
                    process_id: format!("shell-snapshot-warmup-{}", Uuid::new_v4()).into(),
                    metadata: None,
                    // This capture-only operation never starts the command.
                    argv: shell.derive_exec_args("", /*use_login_shell*/ true),
                    cwd: environment.cwd().clone(),
                    env_policy: Some(exec_env_policy_from_shell_policy(
                        environment.shell_environment_policy(),
                    )),
                    shell_snapshot: Some(ShellSnapshotRequest {
                        scope_id: format!(
                            "{}:{}",
                            session.thread_id(),
                            environment.selection.environment_id
                        ),
                        shell: ShellInfo {
                            name: shell.name().to_string(),
                            path: shell.shell_path.to_string_lossy().into_owned(),
                        },
                    }),
                    env,
                    tty: false,
                    pipe_stdin: false,
                    arg0: None,
                };

                let result = environment
                    .environment
                    .get_exec_backend()
                    .prewarm_shell_snapshot(params)
                    .await;
                if let Err(err) = result {
                    tracing::debug!("failed to warm shell snapshot: {err}");
                }
            }
        })))
    }
}

pub(super) fn shell_snapshot_request(
    request: &ExecCommandRequest,
    cwd: &PathUri,
    context: &UnifiedExecContext,
) -> Option<ShellSnapshotRequest> {
    if !context.session.features().enabled(Feature::ShellSnapshotV2)
        || !request.turn_environment.shell_snapshot_v2_supported
        || request.turn_environment.selection.cwd != *cwd
        || !request.shell.is_posix_login()
    {
        return None;
    }

    let shell = &request.shell.shell;
    Some(ShellSnapshotRequest {
        scope_id: format!(
            "{}:{}",
            context.session.thread_id(),
            request.turn_environment.selection.environment_id
        ),
        shell: ShellInfo {
            name: shell.name().to_string(),
            path: shell.shell_path.to_string_lossy().into_owned(),
        },
    })
}

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

use crate::session::ThreadEnvironmentDefaults;
use crate::session::turn_context::ShellSnapshotTask;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell_snapshot::ShellSnapshot;

// Reject pathological selected cwd values at the environment-selection boundary.
const MAX_TURN_ENVIRONMENT_CWD_BYTES: usize = 8 * 1024;

/// Records whether a normalized config should follow later thread setting updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnvironmentConfigOrigin {
    Thread,
    Owner,
}

impl EnvironmentConfigOrigin {
    /// Reconstructs the input form so another attachment boundary preserves config ownership.
    pub(crate) fn into_input_selection(
        self,
        mut selection: TurnEnvironmentSelection,
    ) -> TurnEnvironmentSelection {
        if self == Self::Thread {
            selection.config = EnvironmentConfigState::FromThread;
        }
        selection
    }
}

/// Selects the local environment for a thread that did not request any.
pub(crate) fn default_thread_environment_selections(
    cwd: &AbsolutePathBuf,
    workspace_roots: &[AbsolutePathBuf],
) -> Vec<TurnEnvironmentSelection> {
    vec![TurnEnvironmentSelection {
        environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(cwd),
        workspace_roots: workspace_roots.iter().map(PathUri::from_abs_path).collect(),
        config: EnvironmentConfigState::FromThread,
    }]
}

/// Checks that environment IDs are registered and unique and that working directories are not too long.
pub fn validate_environment_ids_and_cwds<'a>(
    environment_manager: &EnvironmentManager,
    environments: impl IntoIterator<Item = (&'a str, &'a PathUri)>,
) -> CodexResult<()> {
    let mut environment_ids = HashSet::new();
    for (environment_id, cwd) in environments {
        if cwd.inferred_native_path_string().len() > MAX_TURN_ENVIRONMENT_CWD_BYTES {
            return Err(CodexErr::InvalidRequest(
                "turn environment working directory exceeds the maximum size".to_string(),
            ));
        }
        if !environment_ids.insert(environment_id) {
            return Err(CodexErr::InvalidRequest(format!(
                "duplicate turn environment id `{environment_id}`"
            )));
        }
        environment_manager
            .get_environment(environment_id)
            .ok_or_else(|| {
                CodexErr::InvalidRequest(format!("unknown turn environment id `{environment_id}`"))
            })?;
    }
    Ok(())
}

/// Uses the current thread defaults when requested and records who owns later config updates.
fn resolve_selection_config(
    mut selection: TurnEnvironmentSelection,
    defaults: &ThreadEnvironmentDefaults,
) -> (TurnEnvironmentSelection, EnvironmentConfigOrigin) {
    let origin = match selection.config {
        EnvironmentConfigState::FromThread => {
            selection.config = EnvironmentConfigState::Ready(defaults.for_selection(&selection));
            EnvironmentConfigOrigin::Thread
        }
        EnvironmentConfigState::Ready(_) => EnvironmentConfigOrigin::Owner,
    };
    (selection, origin)
}

fn refresh_thread_config(environment: &mut TurnEnvironment, defaults: &ThreadEnvironmentDefaults) {
    if environment.config_origin == EnvironmentConfigOrigin::Thread {
        environment.selection.config =
            EnvironmentConfigState::Ready(defaults.for_selection(&environment.selection));
    }
}

pub(crate) struct ThreadEnvironments {
    environment_manager: Arc<EnvironmentManager>,
    local_shell: Shell,
    shell_snapshot: ShellSnapshot,
    state: Mutex<ThreadEnvironmentsState>,
}

struct ThreadEnvironmentsState {
    thread_defaults: ThreadEnvironmentDefaults,
    environments: Vec<TurnEnvironment>,
}

impl ThreadEnvironments {
    pub(crate) fn new(
        environment_manager: Arc<EnvironmentManager>,
        local_shell: Shell,
        thread_defaults: ThreadEnvironmentDefaults,
        shell_snapshot: ShellSnapshot,
        inherited: TurnEnvironmentSnapshot,
    ) -> Self {
        let environments = inherited
            .environments
            .into_iter()
            .map(|mut environment| {
                // Child threads re-infer thread-owned config while preserving owner config.
                refresh_thread_config(&mut environment, &thread_defaults);
                environment
            })
            .collect();
        Self {
            environment_manager,
            local_shell,
            shell_snapshot,
            state: Mutex::new(ThreadEnvironmentsState {
                thread_defaults,
                environments,
            }),
        }
    }

    fn start_shell_snapshot_task(
        shell_snapshot: ShellSnapshot,
        cwd: PathUri,
        shell: Option<Shell>,
        config: &EnvironmentConfig,
    ) -> ShellSnapshotTask {
        let shell_snapshot = shell_snapshot
            .build(cwd, shell, config.shell_environment_policy.clone())
            .boxed()
            .shared();
        drop(tokio::spawn(
            shell_snapshot
                .clone()
                .in_current_span()
                .with_current_subscriber(),
        ));
        shell_snapshot
    }

    /// Replaces the selected list, keeping started environments whose workspace is unchanged.
    pub(crate) fn update_selections(&self, environments: &[TurnEnvironmentSelection]) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let ThreadEnvironmentsState {
            thread_defaults,
            environments: current,
        } = &mut *state;
        let mut seen_environment_ids = HashSet::with_capacity(environments.len());
        let mut next = Vec::with_capacity(environments.len());
        for selected_environment in environments {
            if !seen_environment_ids.insert(selected_environment.environment_id.as_str()) {
                continue;
            }
            let (selection, config_origin) =
                resolve_selection_config(selected_environment.clone(), thread_defaults);
            if let Some(previous) = current
                .iter()
                .find(|environment| environment.selection.has_same_workspace(&selection))
            {
                let mut environment = previous.clone();
                environment.selection = selection;
                environment.config_origin = config_origin;
                next.push(environment);
                continue;
            }

            let environment_id = &selection.environment_id;
            let Some(environment) = self.environment_manager.get_environment(environment_id) else {
                tracing::warn!("skipping unknown turn environment `{environment_id}`");
                continue;
            };
            next.push(self.start_environment(selection, config_origin, environment));
        }
        *current = next;
    }

    fn start_environment(
        &self,
        selection: TurnEnvironmentSelection,
        config_origin: EnvironmentConfigOrigin,
        environment: Arc<Environment>,
    ) -> TurnEnvironment {
        let shell = Some(self.local_shell.clone());
        let mut turn_environment =
            TurnEnvironment::new(selection, config_origin, environment, shell.clone());
        turn_environment.shell_snapshot = Self::start_shell_snapshot_task(
            self.shell_snapshot.clone(),
            turn_environment.selection.cwd.clone(),
            shell,
            turn_environment.config(),
        );
        turn_environment.executor_platform_os = Some(std::env::consts::OS.to_string());
        turn_environment.user_home_dir = PathUri::from_host_native_path("~").ok();
        turn_environment.temporary_directories =
            Some(codex_exec_server::local_temporary_directories());
        turn_environment.shell_snapshot_v2_supported = cfg!(unix);
        turn_environment
    }

    /// Projects canonical selections back to caller input form without losing config ownership.
    pub(crate) fn selections(&self) -> Vec<TurnEnvironmentSelection> {
        self.snapshot().selections()
    }

    pub(crate) fn primary_workspace_roots_for(
        selections: &[TurnEnvironmentSelection],
    ) -> Vec<AbsolutePathBuf> {
        selections
            .first()
            .map(|selection| {
                selection
                    .workspace_roots
                    .iter()
                    .filter_map(|workspace_root| workspace_root.to_abs_path().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Changes the defaults inherited by all selected and later-added environments. A step that
    /// already captured its environments keeps the previous values.
    pub(crate) fn set_active_thread_defaults(&self, defaults: EnvironmentConfig) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let ThreadEnvironmentsState {
            thread_defaults,
            environments,
        } = &mut *state;
        if thread_defaults.common == defaults {
            return;
        }
        thread_defaults.common = defaults;
        for environment in environments.iter_mut() {
            refresh_thread_config(environment, thread_defaults);
        }
    }

    /// Captures the selected environments for one turn or step.
    pub(crate) fn snapshot(&self) -> TurnEnvironmentSnapshot {
        TurnEnvironmentSnapshot {
            environments: self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .environments
                .clone(),
        }
    }

    pub(crate) fn environment_manager(&self) -> Arc<EnvironmentManager> {
        Arc::clone(&self.environment_manager)
    }
}

/// Environment bindings captured for a turn. Internal child threads can retain these bindings
/// without resolving a different set of environments.
#[derive(Clone, Debug, Default)]
pub struct TurnEnvironmentSnapshot {
    pub(crate) environments: Vec<TurnEnvironment>,
}

impl TurnEnvironmentSnapshot {
    pub(crate) fn turn_environments(&self) -> impl Iterator<Item = &TurnEnvironment> {
        self.environments.iter()
    }

    pub(crate) fn primary(&self) -> Option<&TurnEnvironment> {
        self.environments.first()
    }

    /// Returns the first selected environment's host folders.
    pub(crate) fn primary_workspace_roots(&self) -> Vec<AbsolutePathBuf> {
        self.primary_workspace_root_uris()
            .iter()
            .filter_map(|root| root.to_abs_path().ok())
            .collect()
    }

    pub(crate) fn primary_workspace_root_uris(&self) -> &[PathUri] {
        self.primary()
            .map(|environment| environment.selection.workspace_roots.as_slice())
            .unwrap_or_default()
    }

    pub(crate) fn primary_cwd(&self) -> Option<AbsolutePathBuf> {
        self.primary()?.cwd().to_abs_path().ok()
    }

    pub(crate) fn selections(&self) -> Vec<TurnEnvironmentSelection> {
        self.environments
            .iter()
            .map(TurnEnvironment::selection)
            .collect()
    }

    pub(crate) fn primary_filesystem(&self) -> Option<Arc<dyn ExecutorFileSystem>> {
        self.primary()
            .map(|environment| environment.environment.get_filesystem())
    }
}

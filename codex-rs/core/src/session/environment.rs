use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;

use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::session::Session;
use crate::session::session::SessionConfiguration;
use crate::session::session::SessionSettingsUpdate;

/// Defaults for environments that inherit their configuration from the running turn.
pub(crate) struct ThreadEnvironmentDefaults {
    pub(crate) common: EnvironmentConfig,
}

impl ThreadEnvironmentDefaults {
    pub(crate) fn new(common: EnvironmentConfig) -> Self {
        Self { common }
    }

    pub(crate) fn for_selection(&self, selection: &TurnEnvironmentSelection) -> EnvironmentConfig {
        let mut config = self.common.clone();
        config.workspace_roots = selection.workspace_roots.clone();
        config
    }
}

pub(super) fn ensure_configs_stay_owner_provided(
    current: &[TurnEnvironmentSelection],
    proposed: &[TurnEnvironmentSelection],
) -> CodexResult<()> {
    if let Some(environment) = proposed.iter().find(|environment| {
        environment.config == EnvironmentConfigState::FromThread
            && current.iter().any(|current| {
                current.environment_id == environment.environment_id
                    && current.config != EnvironmentConfigState::FromThread
            })
    }) {
        return Err(CodexErr::InvalidRequest(format!(
            "environment `{}` must keep its owner-provided configuration",
            environment.environment_id
        )));
    }
    Ok(())
}

impl Session {
    pub(super) fn apply_session_settings(
        &self,
        current: &SessionConfiguration,
        updates: &SessionSettingsUpdate,
    ) -> CodexResult<SessionConfiguration> {
        if let Some(environments) = &updates.environments {
            ensure_configs_stay_owner_provided(&current.environments, &environments.environments)?;
        }

        current.apply(updates)
    }

    /// Activates the environments accepted for a new task. Configuration may have changed for
    /// those same environments while its settings were being prepared.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "do not change environments while another task is running"
    )]
    pub(super) async fn activate_turn_environments(
        &self,
        configuration: &SessionConfiguration,
    ) -> TurnEnvironmentSnapshot {
        {
            let active = self.active_turn.lock().await;
            let state = self.state.lock().await;
            // Another task may have started while this one was preparing its settings.
            if active.as_ref().is_none_or(|turn| turn.task.is_none()) {
                let mut environments = configuration.environments.clone();
                let latest_environments = &state.session_configuration.environments;
                for environment in &mut environments {
                    if let Some(latest) = latest_environments
                        .iter()
                        .find(|latest| latest.has_same_workspace(environment))
                    {
                        environment.config = latest.config.clone();
                    }
                }
                // Apply the saved defaults even if this turn selects the same environments.
                self.services
                    .turn_environments
                    .set_active_thread_defaults(configuration.inferred_environment_config());
                if environments != self.services.turn_environments.selections() {
                    self.services
                        .turn_environments
                        .update_selections(&environments);
                }
            }
            self.services.turn_environments.snapshot()
        }
    }
}

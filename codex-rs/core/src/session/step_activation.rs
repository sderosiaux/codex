//! Restricted updates to a running turn's immutable settings snapshots.

use super::environment::ensure_configs_stay_owner_provided;
use super::session::Session;
use super::step_settings::ResolvedStepSettings;
use super::step_settings::StepSettingsUpdate;
use crate::environment_selection::validate_environment_ids_and_cwds;
use codex_features::Feature;
use codex_protocol::protocol::TurnSettingsUpdate;
use codex_protocol::protocol::TurnSettingsUpdateOutcome;
use std::sync::Arc;

impl Session {
    /// Publishes settings to the named, originally captured live task, regardless
    /// of task kind. Publication does not propagate to child sessions or require
    /// the task to sample; consumers using initial settings remain unchanged.
    ///
    /// Callers must serialize updates through completion, including model
    /// resolution, so each sparse patch sees the preceding publication.
    pub(super) async fn apply_turn_settings(
        &self,
        turn_id: &str,
        update: TurnSettingsUpdate,
    ) -> TurnSettingsUpdateOutcome {
        let updates_model_settings = update.model.is_some()
            || update.effort.is_some()
            || update.summary.is_some()
            || update.service_tier.is_some();
        let requires_model_switching = updates_model_settings || update.environments.is_none();
        if requires_model_switching && !self.features.enabled(Feature::StepModelSwitching) {
            return TurnSettingsUpdateOutcome::Rejected {
                reason: "turn settings updates require the step_model_switching feature"
                    .to_string(),
            };
        }

        // Capture the running task named by this update and its settings, then release the lock.
        // A task that starts during preparation is never a new target.
        let target = {
            let active = self.active_turn.lock().await;
            active.as_ref().and_then(|active| {
                active.task.as_ref().and_then(|task| {
                    (task.turn_context.sub_id == turn_id && !task.cancellation_token.is_cancelled())
                        .then(|| {
                            (
                                Arc::clone(&task.turn_context),
                                Arc::clone(&task.done),
                                task.turn_context.next_step_settings.load_full(),
                            )
                        })
                })
            })
        };
        let Some((turn_context, task_done, current)) = target else {
            return TurnSettingsUpdateOutcome::TargetUnavailable;
        };
        let TurnSettingsUpdate {
            environments,
            model,
            effort,
            summary,
            service_tier,
        } = update;
        let update = StepSettingsUpdate {
            model,
            effort,
            reasoning_summary: summary,
            service_tier,
            ..Default::default()
        };
        // Build the proposed model settings.
        // Apply only the fields the caller supplied to the settings captured above. The task can
        // progress, finish, or be cancelled while this awaits; we don't hold the update locks here.
        let updated_settings = if updates_model_settings {
            Some(
                self.prepare_step_settings_activation(&current, &update)
                    .await,
            )
        } else {
            None
        };

        // Confirm the task we originally targeted is still running.
        let active = self.active_turn.lock().await;
        let Some(task) = active.as_ref().and_then(|active| active.task.as_ref()) else {
            return TurnSettingsUpdateOutcome::TargetUnavailable;
        };
        // A later task may reuse the same context and turn ID. `done` is a completion signal
        // created for each task, so matching only the ID/context is insufficient. A mismatch
        // abandons the update without retrying or applying it to the replacement.
        if !Arc::ptr_eq(&task.done, &task_done)
            || !Arc::ptr_eq(&task.turn_context, &turn_context)
            || task.cancellation_token.is_cancelled()
        {
            return TurnSettingsUpdateOutcome::TargetUnavailable;
        }

        // If this update omits environments, use what the running turn's manager knows now.
        let current_environments = self.services.turn_environments.selections();
        let proposed = environments.as_deref().unwrap_or(&current_environments);
        let validation = (|| {
            if environments.is_some() {
                validate_environment_ids_and_cwds(
                    &self.services.turn_environments.environment_manager(),
                    proposed
                        .iter()
                        .map(|selection| (selection.environment_id.as_str(), &selection.cwd)),
                )
                .map_err(|error| error.to_string())?;
                ensure_configs_stay_owner_provided(&current_environments, proposed)
                    .map_err(|error| error.to_string())?;
            }
            Ok::<_, String>(())
        })();
        if let Err(reason) = validation {
            return TurnSettingsUpdateOutcome::Rejected { reason };
        }

        // Save changed settings first. The manager then installs any new environment list before
        // waking waiting work. The next step cannot read either until we release this lock.
        if let Some(settings) = updated_settings {
            task.turn_context
                .next_step_settings
                .store(Arc::new(settings));
        }
        if environments.is_some() {
            self.services.turn_environments.update_selections(proposed);
        }
        TurnSettingsUpdateOutcome::Applied
    }

    async fn prepare_step_settings_activation(
        &self,
        current: &ResolvedStepSettings,
        update: &StepSettingsUpdate,
    ) -> ResolvedStepSettings {
        let overrides = self
            .state
            .lock()
            .await
            .session_configuration
            .model_info_overrides
            .clone();
        current
            .apply_update(
                update,
                self.services.models_manager.as_ref(),
                &overrides,
                &self.features,
            )
            .await
    }
}

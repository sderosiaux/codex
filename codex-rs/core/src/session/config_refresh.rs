//! Publishes resolved MCP configuration while preserving session-static settings.

use std::sync::Arc;

use crate::ConfigRefreshOutcome;
use crate::config::Config;
use crate::config::RuntimeConfigRefresh;
use crate::session::Session;
use codex_config::McpServerAuth;
use codex_features::Feature;
use tracing::warn;

impl Session {
    pub(crate) async fn disable_mcp_enterprise_auth(&self) {
        let mut state = self.state.lock().await;
        let mut config = state
            .session_configuration
            .original_config_do_not_use
            .as_ref()
            .clone();
        config.disable_mcp_enterprise_auth();
        state.session_configuration.original_config_do_not_use = Arc::new(config);
        self.mark_mcp_runtime_dirty();
        drop(state);
        self.schedule_mcp_prewarm();
    }

    pub(crate) async fn refresh_runtime_config(
        &self,
        expected_config: Arc<Config>,
        next_config: Config,
    ) -> ConfigRefreshOutcome {
        self.refresh_config(expected_config, next_config, RuntimeConfigRefresh::User)
            .await
    }

    pub(crate) async fn refresh_mcp_config(
        &self,
        expected_config: Arc<Config>,
        next_config: Config,
    ) -> ConfigRefreshOutcome {
        self.refresh_config(expected_config, next_config, RuntimeConfigRefresh::Mcp)
            .await
    }

    pub(super) async fn refresh_config(
        &self,
        expected_config: Arc<Config>,
        next_config: Config,
        scope: RuntimeConfigRefresh,
    ) -> ConfigRefreshOutcome {
        let (mut config, outcome, rejection_kind) =
            match expected_config.resolve_runtime_refresh(&next_config, scope) {
                Ok(config) => (config, ConfigRefreshOutcome::Published, None),
                Err(error) => {
                    let mut config = expected_config.as_ref().clone();
                    if matches!(scope, RuntimeConfigRefresh::Mcp) {
                        // The host-resolved map already carries preserved session overrides.
                        // Keep ordinary-server revocations even when retained layers make the
                        // enterprise merge invalid.
                        let mut servers = next_config.mcp_servers.clone();
                        // Retain enterprise identities so a later corrected refresh cannot
                        // admit an ordinary-auth replacement under a retired server's name.
                        for (name, server) in &expected_config.mcp_servers {
                            if matches!(server.auth, McpServerAuth::EmaAuth)
                                || server.ema_registration().is_some()
                            {
                                let mut retired = server.clone();
                                retired.enabled = false;
                                servers.insert(name.clone(), retired);
                            }
                        }
                        config.mcp_servers = servers;
                    }
                    config.disable_mcp_enterprise_auth();
                    (config, ConfigRefreshOutcome::Rejected, Some(error.kind()))
                }
            };
        if !matches!(scope, RuntimeConfigRefresh::UserFiles) {
            config.features.set_enabled(
                Feature::Mcp20260728,
                next_config.features.enabled(Feature::Mcp20260728),
            );
        }
        let config = Arc::new(config);
        let notify_contributors = !matches!(scope, RuntimeConfigRefresh::Mcp)
            && !self.services.extensions.config_contributors().is_empty();
        let (previous_config, new_config) = {
            let mut state = self.state.lock().await;
            // Loaded snapshots can publish only while their captured owner is current.
            if !Arc::ptr_eq(
                &state.session_configuration.original_config_do_not_use,
                &expected_config,
            ) {
                return ConfigRefreshOutcome::Stale;
            }
            if let Some(rejection_kind) = rejection_kind {
                warn!(
                    ?rejection_kind,
                    "disabling enterprise MCP after rejected configuration refresh"
                );
            }
            if matches!(scope, RuntimeConfigRefresh::UserFiles) {
                self.services.skills_service.clear_cache();
            }
            // A host refresh carries current rollout settings for recording. A
            // legacy file reload uses its original snapshot and bypasses this path.
            if matches!(scope, RuntimeConfigRefresh::User) {
                self.services
                    .executed_tool_calls
                    .refresh(&next_config.features);
            }
            let previous_config = notify_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            state.session_configuration.original_config_do_not_use = Arc::clone(&config);
            self.mark_mcp_runtime_dirty();
            let new_config = notify_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            (previous_config, new_config)
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        self.schedule_mcp_prewarm();
        self.refresh_hooks(config).await;
        outcome
    }
}

//! Resolves refreshable MCP inputs without rebuilding unrelated session configuration.
//! Existing sessions refresh ordinary MCP state without gaining enterprise authority.

use crate::config::Config;
use codex_config::McpServerAuth;
use codex_config::McpServerConfig;
use codex_features::Feature;
use codex_features::FeatureConfigSource;
use codex_features::FeatureOverrides;
use codex_features::Features;
use codex_features::FeaturesToml;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub(crate) enum RuntimeConfigRefresh {
    User,
    Mcp,
    UserFiles,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct McpRefreshToml {
    features: Option<FeaturesToml>,
}

impl Config {
    pub(crate) fn resolve_runtime_refresh(
        &self,
        incoming: &Self,
        scope: RuntimeConfigRefresh,
    ) -> std::io::Result<Self> {
        let layers = match scope {
            RuntimeConfigRefresh::User | RuntimeConfigRefresh::UserFiles => self
                .config_layer_stack
                .with_user_layer_from(&incoming.config_layer_stack),
            RuntimeConfigRefresh::Mcp => Self::layer_stack_preserving_session(
                &self.config_layer_stack,
                &incoming.config_layer_stack,
            )?
            .with_user_layer_from(&self.config_layer_stack),
        };
        let cfg: McpRefreshToml = layers
            .effective_config()
            .try_into()
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        let mut configured_features = Features::from_sources(
            FeatureConfigSource {
                features: cfg.features.as_ref(),
            },
            FeatureConfigSource::default(),
            FeatureOverrides::default(),
        );
        // These operational controls accept host defaults beneath explicit layer values.
        // Enterprise opt-in always comes from trusted layers instead.
        for feature in [
            Feature::SecretAuthStorage,
            Feature::McpOAuthRefreshCoordination,
        ] {
            if matches!(scope, RuntimeConfigRefresh::UserFiles) {
                let explicitly_configured = cfg.features.as_ref().is_some_and(|features| {
                    features
                        .entries()
                        .keys()
                        .any(|key| codex_features::feature_for_key(key) == Some(feature))
                });
                if !explicitly_configured
                    && let Some(enabled) = self.runtime_feature_defaults.get(&feature)
                {
                    configured_features.set_enabled(feature, *enabled);
                }
            } else {
                // A publication wait may have allowed a newer host/user override.
                configured_features.set_enabled(feature, incoming.features.enabled(feature));
            }
        }
        let is_enterprise = |server: &McpServerConfig| {
            matches!(server.auth, McpServerAuth::EmaAuth) || server.ema_registration().is_some()
        };
        // Refresh ordinary servers from the complete host-owned set. Keep the
        // initially admitted EMA configuration until a fresh session is constructed.
        let mut servers: HashMap<_, _> = incoming
            .mcp_servers
            .iter()
            .filter(|(name, server)| {
                !is_enterprise(server) && !self.mcp_servers.get(*name).is_some_and(is_enterprise)
            })
            .map(|(name, server)| (name.clone(), server.clone()))
            .collect();
        servers.extend(
            self.mcp_servers
                .iter()
                .filter(|(_, server)| is_enterprise(server))
                .map(|(name, server)| (name.clone(), server.clone())),
        );
        let mut config = self.clone();
        if !matches!(scope, RuntimeConfigRefresh::UserFiles) {
            config.runtime_feature_defaults = incoming.runtime_feature_defaults.clone();
            config.mcp_optional_startup_grace = incoming.mcp_optional_startup_grace;
            config.mcp_oauth_credentials_store_mode = incoming.mcp_oauth_credentials_store_mode;
        }
        // Refresh ordinary MCP feature values without changing session-static features.
        for feature in [
            Feature::SecretAuthStorage,
            Feature::McpOAuthRefreshCoordination,
        ] {
            config
                .features
                .set_enabled(feature, configured_features.enabled(feature));
        }
        config.mcp_servers = servers;
        // A missing profile also carries the host's fail-closed load result.
        let enterprise_retired = incoming.mcp_enterprise_managed_auth.is_none()
            || !incoming.features.enabled(Feature::UseXaa)
            || (self
                .mcp_servers
                .values()
                .any(|server| is_enterprise(server) && server.enabled)
                && incoming.mcp_enterprise_managed_auth != self.mcp_enterprise_managed_auth)
            || self.mcp_servers.iter().any(|(name, server)| {
                is_enterprise(server)
                    && server.enabled
                    && !incoming.mcp_servers.get(name).is_some_and(|incoming| {
                        incoming.enabled
                            && incoming.auth == server.auth
                            && incoming.transport == server.transport
                            && incoming.oauth_resource == server.oauth_resource
                            && incoming.oauth == server.oauth
                            && incoming.scopes == server.scopes
                            && incoming.ema_registration() == server.ema_registration()
                    })
            })
            || self.mcp_servers.iter().any(|(name, server)| {
                is_enterprise(server)
                    && server.enabled
                    && !config
                        .mcp_servers
                        .get(name)
                        .is_some_and(|server| server.enabled)
            });
        config.config_layer_stack = layers;
        if enterprise_retired {
            config.disable_mcp_enterprise_auth();
        }
        Ok(config)
    }

    /// Disables enterprise MCP authorization while preserving unrelated MCP servers.
    pub fn disable_mcp_enterprise_auth(&mut self) {
        self.mcp_enterprise_managed_auth = None;
        for server in self.mcp_servers.values_mut() {
            if matches!(server.auth, McpServerAuth::EmaAuth) || server.ema_registration().is_some()
            {
                server.enabled = false;
                if let Some(oauth) = &mut server.oauth {
                    oauth.ema_registration = None;
                }
            }
        }
    }
}

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;

use codex_config::McpServerConfig;
use codex_config::McpServerIdpOAuthConfig;

use crate::McpProtocolMode;

/// The component that declared an MCP server registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerSource {
    Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RegistrationPrecedence {
    Config,
}

impl RegistrationPrecedence {
    fn tier(self) -> u8 {
        match self {
            Self::Config => 0,
        }
    }
}

/// One named MCP server declaration before source resolution.
#[derive(Clone, Debug, PartialEq)]
pub struct McpServerRegistration {
    name: String,
    source: McpServerSource,
    config: McpServerConfig,
    protocol_mode: Option<McpProtocolMode>,
    precedence: RegistrationPrecedence,
}

impl McpServerRegistration {
    /// Registers host-owned configuration, which may resolve credentials on the host.
    pub fn from_config(name: String, config: McpServerConfig) -> Self {
        Self::new(
            name,
            McpServerSource::Config,
            config,
            RegistrationPrecedence::Config,
        )
    }

    fn new(
        name: String,
        source: McpServerSource,
        config: McpServerConfig,
        precedence: RegistrationPrecedence,
    ) -> Self {
        Self {
            name,
            source,
            config,
            protocol_mode: None,
            precedence,
        }
    }
}

/// One side of an MCP server conflict, including whether it registers or
/// removes the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpServerConflictAction {
    Register(McpServerSource),
}

/// A same-tier name collision and the final outcome after all precedence is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConflict {
    pub name: String,
    pub outcome: McpServerConflictAction,
    pub contenders: Vec<McpServerConflictAction>,
}

#[derive(Clone, Debug)]
enum CatalogAction {
    Register(Box<McpServerRegistration>),
}

impl CatalogAction {
    fn name(&self) -> &str {
        match self {
            Self::Register(registration) => &registration.name,
        }
    }

    fn precedence(&self) -> RegistrationPrecedence {
        match self {
            Self::Register(registration) => registration.precedence,
        }
    }

    fn conflict_action(&self) -> McpServerConflictAction {
        match self {
            Self::Register(registration) => {
                McpServerConflictAction::Register(registration.source.clone())
            }
        }
    }
}

/// Mutable inputs used to produce an immutable resolved catalog.
#[derive(Clone, Debug, Default)]
pub struct McpCatalogBuilder {
    actions: Vec<CatalogAction>,
    disabled_server_names: BTreeSet<String>,
    ema_idp: Option<McpServerIdpOAuthConfig>,
}

impl McpCatalogBuilder {
    /// Enables EMA with the IdP selected from trusted configuration.
    /// Without this policy, finalization disables EMA registrations.
    pub fn enable_ema(&mut self, idp: McpServerIdpOAuthConfig) {
        self.ema_idp = Some(idp);
    }

    pub fn register(&mut self, registration: McpServerRegistration) {
        self.actions
            .push(CatalogAction::Register(Box::new(registration)));
    }

    /// Applies the legacy name-scoped disabled veto after source resolution.
    pub fn disable(&mut self, name: String) {
        self.disabled_server_names.insert(name);
    }

    pub fn build(mut self) -> ResolvedMcpCatalog {
        // Keep source actions unbound so later catalog revisions resolve afresh.
        for action in &mut self.actions {
            if let CatalogAction::Register(registration) = action
                && let Some(oauth) = &mut registration.config.oauth
            {
                oauth.ema_registration = None;
            }
        }
        // Stable sorting makes action order the tie-breaker when precedence is equal.
        self.actions.sort_by_key(CatalogAction::precedence);

        let mut winners = BTreeMap::<String, CatalogAction>::new();
        let mut actions_by_name_and_tier = BTreeMap::<(String, u8), Vec<&CatalogAction>>::new();
        for action in &self.actions {
            winners.insert(action.name().to_string(), action.clone());
            actions_by_name_and_tier
                .entry((action.name().to_string(), action.precedence().tier()))
                .or_default()
                .push(action);
        }

        let mut conflicts = Vec::new();
        for ((name, _), actions) in actions_by_name_and_tier {
            if actions.len() < 2 {
                continue;
            }
            let Some(outcome) = winners.get(&name).map(CatalogAction::conflict_action) else {
                continue;
            };
            conflicts.push(McpServerConflict {
                name,
                outcome,
                contenders: actions
                    .into_iter()
                    .map(CatalogAction::conflict_action)
                    .collect(),
            });
        }

        let mut disabled_server_names = self.disabled_server_names;
        let ema_idp = self.ema_idp;
        let servers = winners
            .into_iter()
            .map(|(name, CatalogAction::Register(registration))| {
                let mut registration = *registration;
                if !registration.config.enabled || disabled_server_names.contains(&name) {
                    registration.config.enabled = false;
                    // Preserve legacy disabled winners across later runtime overlays.
                    disabled_server_names.insert(name.clone());
                }
                if matches!(
                    registration.config.auth,
                    codex_config::McpServerAuth::EmaAuth
                ) {
                    let allowed = matches!(&registration.source, McpServerSource::Config)
                        && ema_idp.as_ref().is_some_and(|idp| {
                            registration.config.resolve_ema_registration(idp).is_ok()
                        });
                    // EMA denial must not become a persistent name veto.
                    registration.config.enabled &= allowed;
                }
                (
                    name,
                    ResolvedMcpServer {
                        source: registration.source,
                        config: registration.config,
                        protocol_mode: registration.protocol_mode,
                    },
                )
            })
            .collect();

        ResolvedMcpCatalog {
            actions: self.actions,
            disabled_server_names,
            ema_idp,
            servers,
            conflicts,
        }
    }
}

/// A single winning MCP registration.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMcpServer {
    source: McpServerSource,
    config: McpServerConfig,
    protocol_mode: Option<McpProtocolMode>,
}

impl ResolvedMcpServer {
    pub fn source(&self) -> &McpServerSource {
        &self.source
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn protocol_mode(&self) -> Option<McpProtocolMode> {
        self.protocol_mode
    }
}

/// Immutable result of MCP registration resolution.
#[derive(Clone, Debug, Default)]
pub struct ResolvedMcpCatalog {
    actions: Vec<CatalogAction>,
    disabled_server_names: BTreeSet<String>,
    ema_idp: Option<McpServerIdpOAuthConfig>,
    servers: BTreeMap<String, ResolvedMcpServer>,
    conflicts: Vec<McpServerConflict>,
}

impl ResolvedMcpCatalog {
    pub fn builder() -> McpCatalogBuilder {
        McpCatalogBuilder::default()
    }

    pub fn to_builder(&self) -> McpCatalogBuilder {
        McpCatalogBuilder {
            actions: self.actions.clone(),
            disabled_server_names: self.disabled_server_names.clone(),
            ema_idp: self.ema_idp.clone(),
        }
    }

    pub fn server(&self, name: &str) -> Option<&ResolvedMcpServer> {
        self.servers.get(name)
    }

    pub fn configured_servers(&self) -> HashMap<String, McpServerConfig> {
        self.servers
            .iter()
            .map(|(name, server)| (name.clone(), server.config.clone()))
            .collect()
    }

    /// Returns whether both catalogs have the same winning servers, sources, and EMA policy.
    pub fn has_same_servers(&self, other: &Self) -> bool {
        self.servers == other.servers && self.ema_idp == other.ema_idp
    }

    /// Replaces the resolved server set while preserving known sources and EMA policy.
    ///
    /// # Panics
    ///
    /// Panics if a materialized server is missing from the catalog.
    pub fn with_materialized_servers(&self, servers: HashMap<String, McpServerConfig>) -> Self {
        let mut builder = McpCatalogBuilder {
            ema_idp: self.ema_idp.clone(),
            ..Default::default()
        };
        for (name, config) in servers {
            #[expect(
                clippy::expect_used,
                reason = "materialized servers must have catalog registrations"
            )]
            let previous = self
                .server(&name)
                .expect("materialized MCP server must have a catalog registration");
            let source = previous.source.clone();
            let precedence = match &source {
                McpServerSource::Config => RegistrationPrecedence::Config,
            };
            let mut registration = McpServerRegistration::new(name, source, config, precedence);
            registration.protocol_mode = previous.protocol_mode();
            builder.register(registration);
        }
        builder.build()
    }

    pub fn conflicts(&self) -> &[McpServerConflict] {
        &self.conflicts
    }
}

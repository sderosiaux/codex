use crate::config::Config;
use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_mcp::McpConfig;
use codex_mcp::McpEnvironmentAuthority;
use codex_mcp::McpToolCatalogCache;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;

pub(crate) fn environment_authority(
    selections: &[TurnEnvironmentSelection],
    environment_id: &str,
) -> McpEnvironmentAuthority {
    let Some(selection) = selections
        .iter()
        .find(|selection| selection.environment_id == environment_id)
    else {
        return if environment_id == DEFAULT_MCP_SERVER_ENVIRONMENT_ID {
            McpEnvironmentAuthority::Unrestricted
        } else {
            McpEnvironmentAuthority::Unavailable
        };
    };
    match &selection.config {
        EnvironmentConfigState::FromThread | EnvironmentConfigState::Ready(_) => {
            McpEnvironmentAuthority::Unrestricted
        }
        EnvironmentConfigState::Pending | EnvironmentConfigState::Failed(_) => {
            McpEnvironmentAuthority::Unavailable
        }
    }
}

#[derive(Clone, Default)]
pub struct McpManager {
    tool_catalog_cache: McpToolCatalogCache,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_catalog_cache(&self) -> McpToolCatalogCache {
        self.tool_catalog_cache.clone()
    }

    /// Applies the thread's environment selection to the configured MCP servers.
    #[tracing::instrument(name = "mcp.runtime_config.project_for_step", skip_all)]
    pub(crate) fn runtime_config_for_step(
        &self,
        config: &Config,
        environment_selections: &[TurnEnvironmentSelection],
    ) -> McpConfig {
        let mut mcp_config = config.to_mcp_config();
        let catalog = mcp_config
            .mcp_server_catalog
            .to_builder()
            .build_with_environment_authority(|environment_id| {
                environment_authority(environment_selections, environment_id)
            });
        for conflict in catalog.conflicts() {
            tracing::warn!(
                server = conflict.name,
                outcome = ?conflict.outcome,
                contenders = ?conflict.contenders,
                "conflicting MCP server actions; using resolved catalog outcome"
            );
        }
        mcp_config.mcp_server_catalog = catalog;
        mcp_config
    }
}

use crate::config::Config;
use codex_mcp::McpConfig;
use codex_mcp::McpToolCatalogCache;

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

    /// Resolves the configured MCP servers for one step.
    #[tracing::instrument(name = "mcp.runtime_config.project_for_step", skip_all)]
    pub(crate) fn runtime_config_for_step(&self, config: &Config) -> McpConfig {
        let mut mcp_config = config.to_mcp_config();
        let catalog = mcp_config.mcp_server_catalog.to_builder().build();
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

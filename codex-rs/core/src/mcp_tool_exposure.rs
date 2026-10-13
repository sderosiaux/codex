use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use codex_mcp::McpBinding;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use tracing::instrument;
use tracing::warn;

use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolRegistry;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    handlers: Mutex<HashMap<ToolName, CachedMcpHandler>>,
}

struct CachedMcpHandler {
    schema_max_bytes: Option<NonZeroUsize>,
    handler: Arc<McpHandler>,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut handlers = self
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        append_mcp_tools(
            binding.tools(),
            mcp_server_catalog,
            search_tool_enabled,
            &mut handlers,
            registry,
        )
    }
}

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
    handlers: &mut HashMap<ToolName, CachedMcpHandler>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    let current_tools: HashSet<_> = all_mcp_tools
        .iter()
        .map(McpToolInfo::canonical_tool_name)
        .collect();
    handlers.retain(|name, _| current_tools.contains(name));

    let exposure = if search_tool_enabled {
        ToolExposure::Deferred
    } else {
        ToolExposure::Direct
    };
    let mut registered_tools = HashSet::new();
    for tool in all_mcp_tools
        .iter()
        .filter(|tool| tool_is_model_visible(tool))
    {
        let tool_name = tool.canonical_tool_name();
        let tool_input_schema_max_bytes = mcp_server_catalog
            .server(&tool.server_name)
            .and_then(|server| server.config().tool_input_schema_max_bytes);
        // Handlers contain immutable tool metadata, not a connection or authorization snapshot.
        // Preserve their identity across equivalent bindings so the search index can also be reused.
        let handler = if let Some(cached) = handlers.get(&tool_name).filter(|cached| {
            cached.handler.tool_info() == tool
                && cached.schema_max_bytes == tool_input_schema_max_bytes
        }) {
            Arc::clone(&cached.handler)
        } else {
            handlers.remove(&tool_name);
            let handler = if let Some(budget) = tool_input_schema_max_bytes {
                McpHandler::new_with_schema_max_bytes(tool.clone(), budget.get())
            } else {
                McpHandler::new(tool.clone())
            };

            let handler = match handler {
                Ok(handler) => Arc::new(handler),
                Err(err) => {
                    warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                    continue;
                }
            };
            handlers.insert(
                tool_name.clone(),
                CachedMcpHandler {
                    schema_max_bytes: tool_input_schema_max_bytes,
                    handler: Arc::clone(&handler),
                },
            );
            handler
        };

        if registry.register_external_with_exposure(handler, exposure) {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

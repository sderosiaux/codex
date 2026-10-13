use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use codex_config::McpStartupReadiness;
use futures::future::join_all;
use tracing::Instrument;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;

use super::McpConnectionSet;
use super::McpServerMetadata;
use super::catalog_telemetry::emit_binding_catalog;
use super::catalog_telemetry::record_binding_catalog_size;
use super::catalog_telemetry::tool_definition_json_bytes;
use crate::binding::McpBinding;
use crate::binding::PreparedMcpCall;
use crate::binding::PreparedToolInfo;
use crate::binding_clients::McpBindingClients;
use crate::client_tool_catalog::ClientToolCatalogRevision;
use crate::client_tool_catalog::ToolCatalogSnapshot;
use crate::rmcp_client::ManagedClient;
use crate::tools::ToolInfo;
use crate::tools::filter_tools;
use crate::tools::normalize_tools_for_model_with_prefix;

const MCP_UI_META_KEY: &str = "ui";
const MCP_UI_VISIBILITY_META_KEY: &str = "visibility";
const MCP_UI_MODEL_VISIBILITY: &str = "model";
/// Returns whether a tool may be included in model-facing tool declarations.
///
/// Tools without visibility metadata remain visible. Tools with visibility
/// metadata are hidden unless they explicitly include `model`.
///
/// <https://github.com/modelcontextprotocol/ext-apps/blob/main/specification/2026-01-26/apps.mdx#resource-discovery>
pub fn tool_is_model_visible(tool: &ToolInfo) -> bool {
    let Some(visibility) = tool
        .tool
        .meta
        .as_deref()
        .and_then(|meta| meta.get(MCP_UI_META_KEY))
        .and_then(serde_json::Value::as_object)
        .and_then(|ui| ui.get(MCP_UI_VISIBILITY_META_KEY))
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    visibility
        .iter()
        .any(|target| target.as_str() == Some(MCP_UI_MODEL_VISIBILITY))
}

/// Catalog identity within one published connection set, including cached declarations.
#[derive(PartialEq, Eq)]
pub(crate) enum BindingCatalogRevision {
    Ready(ClientToolCatalogRevision),
    Cached(u64),
}

enum PendingCallToolInfo {
    BindingCatalog(usize),
    Owned(PreparedToolInfo),
}

impl McpConnectionSet {
    /// Observes opted-in servers at startup gates without waiting or cloning catalogs.
    pub(crate) fn record_startup_readiness(
        &self,
        phase: &'static str,
        required_servers: &[String],
    ) {
        if !tracing::enabled!(tracing::Level::INFO) {
            return;
        }
        for (server_name, view) in &self.servers {
            if view.startup_readiness != McpStartupReadiness::Catalog {
                continue;
            }
            let client = &view.connection.client;
            let startup_pending = !client.startup_complete.load(Ordering::Acquire);
            let cache_eligible = startup_pending
                && client
                    .tool_catalog_cache_context
                    .as_ref()
                    .is_some_and(|cache| {
                        cache
                            .current_revision_if(|tools| view.accepts_cached_tools(tools))
                            .is_some()
                    });
            let explicitly_required = required_servers.contains(server_name);
            tracing::info!(
                event.name = "mcp.startup_readiness",
                server_name = %server_name,
                phase,
                startup_pending,
                startup_readiness = "catalog",
                cache_eligible,
                required = self.required_servers.binary_search(server_name).is_ok(),
                explicitly_required,
                "MCP startup readiness observed"
            );
        }
    }

    pub(crate) async fn stable_catalog_revisions(
        &self,
        required_servers: &[String],
    ) -> Option<HashMap<String, BindingCatalogRevision>> {
        let mut revisions = HashMap::new();
        for (server_name, view) in &self.servers {
            let startup_complete = view
                .connection
                .client
                .startup_complete
                .load(Ordering::Acquire);
            if !startup_complete
                || (view.startup_readiness == McpStartupReadiness::Catalog
                    && view.connection.client.ready_client().is_none())
            {
                // Explicit requirements still wait for a live connection during capture.
                if !view.allows_cached_startup()
                    || view.connection.client.cancel_token.is_cancelled()
                    || required_servers
                        .iter()
                        .any(|required| required == server_name)
                {
                    return None;
                }
                let revision = view
                    .connection
                    .client
                    .tool_catalog_cache_context
                    .as_ref()?
                    .current_revision_if(|tools| view.accepts_cached_tools(tools))?;
                revisions.insert(
                    server_name.clone(),
                    BindingCatalogRevision::Cached(revision),
                );
                continue;
            }
            let Some(client) = view.connection.client.ready_client() else {
                if self.required_servers.binary_search(server_name).is_err()
                    && matches!(view.connection.client.client.peek(), Some(Err(_)))
                {
                    continue;
                }
                return None;
            };
            if client.client.is_closed().await {
                return None;
            }
            let revision = client.tool_catalog.read(|catalog| catalog.revision).await;
            revisions.insert(
                server_name.clone(),
                BindingCatalogRevision::Ready(ClientToolCatalogRevision {
                    catalog: Arc::clone(&client.tool_catalog),
                    revision,
                }),
            );
        }
        Some(revisions)
    }

    /// Returns all tools with model-visible names normalized.
    pub async fn list_all_tools(&self) -> Vec<ToolInfo> {
        Box::pin(self.list_tools_with_errors(|_| true)).await.0
    }

    #[instrument(level = "trace", skip_all, fields(mcp_server_count = self.servers.len()))]
    pub(crate) async fn list_tools_with_errors(
        &self,
        include_server: impl Fn(&str) -> bool,
    ) -> (Vec<ToolInfo>, HashMap<String, String>) {
        let mut tools = Vec::new();
        let mut errors = HashMap::new();
        let mut available_server_count = 0;
        let mut unavailable_server_count = 0;
        let server_results = join_all(
            self.servers
                .iter()
                .filter(|(name, _)| include_server(name))
                .map(|(server_name, view)| async move {
                    let has_cached_tools = view.connection.client.has_cached_tools();
                    let startup_complete = view
                        .connection
                        .client
                        .startup_complete
                        .load(Ordering::Acquire);
                    let server_tools = view
                        .listed_tools()
                        .instrument(trace_span!(
                            "list_tools_for_server",
                            server_name = %server_name,
                            has_cached_tools,
                            startup_complete
                        ))
                        .await;
                    let result = match server_tools {
                        Ok(server_tools) => Ok(server_tools
                            .into_iter()
                            .map(|tool| Self::with_server_metadata(tool, &view.metadata))
                            .collect::<Vec<_>>()),
                        Err(error) => {
                            trace!(
                                server_name = %server_name,
                                has_cached_tools,
                                startup_complete,
                                "MCP server tools unavailable while building tool list"
                            );
                            Err(error)
                        }
                    };
                    (server_name, result)
                }),
        )
        .await;
        for (server_name, server_tools) in server_results {
            match server_tools {
                Ok(server_tools) => {
                    available_server_count += 1;
                    tools.extend(server_tools);
                }
                Err(error) => {
                    unavailable_server_count += 1;
                    errors.insert(server_name.clone(), error.to_string());
                }
            }
        }
        let tools = normalize_tools_for_model_with_prefix(
            tools,
            self.prefix_mcp_tool_names,
            &self.non_prefixed_mcp_tool_servers,
        );
        trace!(
            available_server_count,
            unavailable_server_count,
            tool_count = tools.len(),
            "built MCP tool list"
        );
        (tools, errors)
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn capture_binding_with_metadata(
        self: &Arc<Self>,
        config: Arc<crate::McpConfig>,
        required_servers: &[String],
    ) -> McpBinding {
        let mut listed_tools = Vec::new();
        let mut clients = HashMap::new();
        let catalog_log_enabled = tracing::enabled!(
            target: "codex_otel.trace_safe",
            tracing::Level::INFO
        );
        let catalog_metrics = codex_otel::global();
        let catalog_measurement_enabled = catalog_log_enabled || catalog_metrics.is_some();
        let product_sku = codex_otel::bounded_product_sku(Some("codex")).unwrap_or("unknown");
        let optional_mcp_startup_grace = config.optional_mcp_startup_grace;
        let server_snapshots = join_all(self.servers.iter().map(|(server_name, view)| async move {
            if !view
                .connection
                .client
                .startup_complete
                .load(Ordering::Acquire)
            {
                let required = self.required_servers.binary_search(server_name).is_ok();
                // Keep the catalog that lets us skip startup even if it expires during the wait.
                let cached_tools = view.cached_startup_tools(/*fallback*/ None);
                let has_cached_tools = cached_tools.is_some();
                let must_wait_for_startup = (required
                    && (!view.allows_cached_startup() || !has_cached_tools))
                    || required_servers
                        .iter()
                        .any(|required| required == server_name);
                if !must_wait_for_startup && has_cached_tools {
                    trace!(server_name = %server_name, "using cached MCP catalog without waiting for startup");
                    return (server_name, view, cached_tools);
                }
                if !must_wait_for_startup && optional_mcp_startup_grace.is_zero() {
                    if let Some(cache) = view.connection.client.tool_catalog_cache_context.as_ref()
                    {
                        cache.optional_startup_deadline(
                            tokio::time::Instant::now(),
                            optional_mcp_startup_grace,
                        );
                    }
                    let _ = view.connection.client().await;
                } else if !must_wait_for_startup {
                    let optional_startup_deadline = if view.connection.startup_is_dormant() {
                        tokio::time::Instant::now() + optional_mcp_startup_grace
                    } else {
                        *self.optional_startup_deadline.get_or_init(|| {
                            tokio::time::Instant::now() + optional_mcp_startup_grace
                        })
                    };
                    let startup_deadline = view
                        .connection
                        .client
                        .tool_catalog_cache_context
                        .as_ref()
                        .map(|cache| {
                            cache.optional_startup_deadline(
                                optional_startup_deadline,
                                optional_mcp_startup_grace,
                            )
                        })
                        .unwrap_or(optional_startup_deadline);
                    if tokio::time::timeout_at(startup_deadline, view.connection.client())
                        .await
                        .is_err()
                    {
                        trace!(server_name = %server_name, "omitting pending optional MCP server");
                    }
                    return (server_name, view, cached_tools);
                }
                let _ = view.connection.client().await;
                return (server_name, view, cached_tools);
            }
            (server_name, view, None)
        }))
        .await;
        let server_results = join_all(server_snapshots.into_iter().map(|(server_name, view, cached_tools)| async move {
            let startup_pending = !view
                .connection
                .client
                .startup_complete
                .load(Ordering::Acquire);
            let accepts_cached_catalog = view.startup_readiness == McpStartupReadiness::Catalog;
            let cached_tools = if startup_pending
                || (accepts_cached_catalog && view.connection.client.ready_client().is_none())
            {
                view.cached_startup_tools(cached_tools)
            } else {
                None
            };
            let (client, server_tools, catalog_source) = if let Some(cached_tools) = cached_tools {
                (None, cached_tools, "cached")
            } else {
                // A server may opt out of caching after the first pass. Required catalog
                // readiness must then wait for discovery rather than silently omit its tools.
                if startup_pending
                    && !(accepts_cached_catalog
                        && self.required_servers.binary_search(server_name).is_ok())
                {
                    return None;
                }
                let Ok(mut client) = view.connection.client().await else {
                    trace!(server_name = %server_name, "omitting MCP server without an exact ready client");
                    return None;
                };
                client.tool_timeout = view.tool_timeout;
                let snapshot = client.tool_catalog.read(Arc::new).await;
                let server_tools = snapshot.tools.to_vec();
                (
                    Some((Arc::new(client), snapshot)),
                    server_tools,
                    "live",
                )
            };
            let raw_definition_json_bytes = if catalog_measurement_enabled {
                tool_definition_json_bytes(server_tools.iter().map(|tool| &tool.tool))
            } else {
                0
            };
            if catalog_log_enabled {
                emit_binding_catalog(
                    product_sku,
                    "configured",
                    catalog_source,
                    server_tools.len(),
                    raw_definition_json_bytes,
                );
            }
            let server_tools = filter_tools(server_tools, &view.tool_filter)
                .into_iter()
                .map(|mut tool| {
                    if client.is_none()
                        && let Some(annotations) = tool.tool.annotations.as_mut()
                    {
                        annotations.read_only_hint = None;
                    }
                    Self::with_server_metadata(tool, &view.metadata)
                })
                .collect::<Vec<_>>();
            Some((
                server_name.clone(),
                client,
                server_tools,
                raw_definition_json_bytes,
            ))
        }))
        .await;
        let mut raw_definition_json_bytes = 0usize;
        for (server_name, client, server_tools, server_definition_json_bytes) in
            server_results.into_iter().flatten()
        {
            raw_definition_json_bytes =
                raw_definition_json_bytes.saturating_add(server_definition_json_bytes);
            if let Some((client, snapshot)) = client {
                clients.insert(server_name, (client, snapshot));
            }
            listed_tools.extend(server_tools);
        }
        if let Some(metrics) = catalog_metrics.as_ref() {
            record_binding_catalog_size(metrics, product_sku, raw_definition_json_bytes);
        }
        let listed_tools = normalize_tools_for_model_with_prefix(
            listed_tools,
            self.prefix_mcp_tool_names,
            &self.non_prefixed_mcp_tool_servers,
        );
        let mut tools = Vec::with_capacity(listed_tools.len());
        let mut pending_calls = Vec::with_capacity(listed_tools.len());
        for tool_info in listed_tools {
            let model_visible = crate::tool_is_model_visible(&tool_info);
            let Some((client, snapshot)) = clients.get(&tool_info.server_name) else {
                if model_visible {
                    tools.push(tool_info);
                }
                continue;
            };
            let pending_tool_info = if model_visible {
                let tool_index = tools.len();
                tools.push(tool_info);
                PendingCallToolInfo::BindingCatalog(tool_index)
            } else {
                PendingCallToolInfo::Owned(tool_info.into())
            };
            pending_calls.push((pending_tool_info, Arc::clone(client), Arc::clone(snapshot)));
        }
        let tools: Arc<[ToolInfo]> = tools.into();
        let mut calls = std::collections::HashMap::with_capacity(pending_calls.len());
        for (pending_tool_info, client, snapshot) in pending_calls {
            let tool_info = match pending_tool_info {
                PendingCallToolInfo::BindingCatalog(tool_index) => {
                    PreparedToolInfo::from_binding_catalog(Arc::clone(&tools), tool_index)
                }
                PendingCallToolInfo::Owned(tool_info) => tool_info,
            };
            let Some(call) = self.prepare_call(tool_info, client, Arc::clone(&config), snapshot)
            else {
                continue;
            };
            calls.insert(
                (
                    call.server_name().to_string(),
                    call.tool_info().tool.name.to_string(),
                ),
                call,
            );
        }
        let clients = Arc::new(McpBindingClients::new(
            clients
                .into_iter()
                .map(|(server_name, (client, _))| (server_name, client))
                .collect(),
        ));
        McpBinding::new(Arc::clone(self), clients, config, tools, calls)
    }

    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn prepare_call_for_tool(
        self: &Arc<Self>,
        config: Arc<crate::McpConfig>,
        advertised_tool: &ToolInfo,
    ) -> Option<PreparedMcpCall> {
        let server_name = &advertised_tool.server_name;
        let view = self.servers.get(server_name)?;
        if !view.tool_filter.allows(&advertised_tool.tool.name) {
            return None;
        }
        let mut client = view.connection.client().await.ok()?;
        client.tool_timeout = view.tool_timeout;
        let snapshot = client.tool_catalog.read(Arc::new).await;
        let mut tool_info = snapshot
            .tools
            .iter()
            .find(|tool| {
                tool.server_name == *server_name && tool.tool.name == advertised_tool.tool.name
            })?
            .clone();
        if !tool_is_model_visible(&tool_info) {
            return None;
        }
        tool_info = Self::with_server_metadata(tool_info, &view.metadata);
        // Preserve the globally normalized identity advertised to the model, while
        // taking schema and annotations from the current catalog.
        tool_info
            .callable_namespace
            .clone_from(&advertised_tool.callable_namespace);
        tool_info
            .callable_name
            .clone_from(&advertised_tool.callable_name);
        self.prepare_call(tool_info, Arc::new(client), config, snapshot)
    }

    fn prepare_call(
        self: &Arc<Self>,
        tool_info: impl Into<PreparedToolInfo>,
        client: Arc<ManagedClient>,
        config: Arc<crate::McpConfig>,
        tool_catalog_snapshot: Arc<ToolCatalogSnapshot>,
    ) -> Option<PreparedMcpCall> {
        let tool_info = tool_info.into();
        let server_metadata = self
            .servers
            .get(&tool_info.get().server_name)?
            .metadata
            .clone();
        Some(PreparedMcpCall::new(
            client,
            config,
            tool_catalog_snapshot,
            tool_info,
            server_metadata,
        ))
    }

    fn with_server_metadata(mut tool: ToolInfo, metadata: &McpServerMetadata) -> ToolInfo {
        tool.supports_parallel_tool_calls = metadata.supports_parallel_tool_calls;
        tool.server_origin = metadata
            .origin
            .as_ref()
            .map(|origin| origin.as_str().to_string());
        tool
    }
}

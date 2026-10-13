//! RMCP client lifecycle for MCP server connections.
//!
//! This module owns startup of individual RMCP clients: building the transport,
//! initializing the server, listing raw tools, and applying per-server tool filters.
//! Initialization capabilities survive tool-discovery failures and reset on a new attempt.
//! Executor-discovered environment credentials remain executor-bound across those attempts.
//! Higher-level aggregation and resource/tool APIs live in
//! [`crate::connection_manager`].

#[path = "rmcp_client/status.rs"]
mod status;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::client_tool_catalog::ClientToolCatalog;
use crate::elicitation::ElicitationRequestManager;
use crate::openai_docs_source_attribution::maybe_with_openai_docs_source_attribution;
use crate::pagination::collect_paginated_with_limit;
use crate::runtime::McpRuntimeContext;
use crate::runtime::emit_duration;
use crate::server::EffectiveMcpServer;
use crate::tool_catalog_cache::McpToolCatalogCacheContext;
use crate::tool_catalog_cache::McpToolCatalogFetchTicket;
use crate::tools::ToolInfo;
use anyhow::Result;
use anyhow::anyhow;
use async_channel::Sender;
use codex_api::SharedAuthProvider;
use codex_async_utils::CancelErr;
use codex_async_utils::OrCancelExt;
use codex_config::McpServerAuth;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_login::AuthChangeState;
use codex_otel::auth_storage::AuthStorageOriginator;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::mcp::McpServerInfo;
use codex_protocol::protocol::Event;
use codex_rmcp_client::LocalStdioServerLauncher;
use codex_rmcp_client::McpOAuthRefreshMode;
use codex_rmcp_client::McpProtocolMode;
use codex_rmcp_client::RmcpClient;
use codex_rmcp_client::StdioServerLauncher;
use codex_rmcp_client::StreamableHttpBearerToken;
use codex_rmcp_client::StreamableHttpRedirectMode;
use codex_rmcp_client::is_authentication_required_error;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures::future::Shared;
use rmcp::model::ClientCapabilities;
use rmcp::model::ElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use rmcp::model::ServerPeerInfo;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument;
use tracing::warn;

/// Experimental MCP server capability for development and testing only; production servers should
/// not use it. Its `cacheable: false` property disables sharing tool definitions across connections.
const MCP_TOOL_CATALOG_CACHE_CAPABILITY: &str = "codex/tool-catalog-cache";
const MCP_TOOL_CATALOG_CACHEABLE_PROPERTY: &str = "cacheable";
pub(crate) const MCP_TOOLS_LIST_DURATION_METRIC: &str = "codex.mcp.tools.list.duration_ms";
pub(crate) const MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC: &str =
    "codex.mcp.tools.fetch_uncached.duration_ms";
pub(crate) const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub(crate) struct ManagedClient {
    pub(crate) _auth_change_notifications: Option<Arc<AbortOnDropHandle<()>>>,
    pub(crate) client: Arc<RmcpClient>,
    pub(crate) server_info: McpServerInfo,
    pub(crate) tool_catalog: Arc<ClientToolCatalog>,
    pub(crate) tool_timeout: Option<Duration>,
}

impl ManagedClient {
    pub(crate) async fn listed_tools(&self) -> Vec<ToolInfo> {
        self.tool_catalog
            .read(|catalog| catalog.tools.to_vec())
            .await
    }
}

pub(crate) type ManagedClientFuture =
    Shared<BoxFuture<'static, Result<ManagedClient, StartupOutcomeError>>>;

#[derive(Clone)]
struct ManagedClientStartup {
    originator: AuthStorageOriginator,
    server_name: String,
    server: EffectiveMcpServer,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    oauth_refresh_mode: McpOAuthRefreshMode,
    tx_event: Option<Sender<Event>>,
    elicitation_requests: ElicitationRequestManager,
    tool_catalog_cache_context: Option<McpToolCatalogCacheContext>,
    runtime_context: McpRuntimeContext,
    runtime_auth_provider: Option<SharedAuthProvider>,
    client_elicitation_capability: ElicitationCapability,
    client_mcp_extensions: ClientMcpExtensions,
    auth_changes: Option<watch::Receiver<AuthChangeState>>,
    protocol_mode: McpProtocolMode,
    catalog_item_limit: usize,
    cancel_token: CancellationToken,
    startup_complete: Arc<AtomicBool>,
    server_capabilities: Arc<StdMutex<Option<serde_json::Value>>>,
}

impl ManagedClientStartup {
    fn start(&self) -> ManagedClientFuture {
        // A new attempt must not expose capabilities from an earlier connection.
        *self
            .server_capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let Self {
            originator,
            server_name,
            server,
            store_mode,
            keyring_backend_kind,
            oauth_refresh_mode,
            tx_event,
            elicitation_requests,
            tool_catalog_cache_context,
            runtime_context,
            runtime_auth_provider,
            client_elicitation_capability,
            client_mcp_extensions,
            auth_changes,
            protocol_mode,
            catalog_item_limit,
            cancel_token,
            startup_complete,
            server_capabilities,
        } = self.clone();
        let startup_timeout = server
            .config()
            .startup_timeout_sec
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT);
        let cancel_token_for_fut = cancel_token;
        let startup = async move {
            let tool_catalog_fetch_ticket = tool_catalog_cache_context
                .as_ref()
                .map(McpToolCatalogCacheContext::begin_fetch);
            let outcome = match async {
                if let Err(error) = validate_mcp_server_name(&server_name) {
                    return Err(error.into());
                }

                let client = match tokio::time::timeout(
                    startup_timeout,
                    make_rmcp_client(
                        &server_name,
                        server.clone(),
                        store_mode,
                        keyring_backend_kind,
                        oauth_refresh_mode,
                        runtime_context,
                        runtime_auth_provider,
                        protocol_mode,
                    ),
                )
                .await
                {
                    Ok(result) => Arc::new(
                        result?.with_read_only_tools(server.requires_read_only_mcp_tools()),
                    ),
                    Err(_) => {
                        return Err(StartupOutcomeError::from(anyhow!(
                            "MCP client startup timed out after {startup_timeout:?}"
                        )));
                    }
                };
                start_server_task(
                    server_name.clone(),
                    client,
                    StartServerTaskParams {
                        startup_timeout: Some(startup_timeout),
                        tx_event,
                        elicitation_requests,
                        tool_catalog_cache_context,
                        tool_catalog_fetch_ticket,
                        client_elicitation_capability,
                        client_mcp_extensions,
                        auth_changes,
                        catalog_item_limit,
                        server_capabilities,
                    },
                )
                .await
            }
            .or_cancel(&cancel_token_for_fut)
            .await
            {
                Ok(result) => result,
                Err(CancelErr::Cancelled) => Err(StartupOutcomeError::Cancelled),
            };
            // Log once per startup attempt, including discovery without startup notifications.
            if let Err(StartupOutcomeError::Failed { error, .. }) = &outcome {
                warn!(server_name, %error, "MCP server startup failed");
            }
            startup_complete.store(true, Ordering::Release);
            outcome
        };
        originator
            .scope(Box::pin(startup))
            .in_current_span()
            .boxed()
            .shared()
    }
}

#[derive(Clone)]
pub(crate) struct AsyncManagedClient {
    pub(crate) client: ManagedClientFuture,
    /// Retained after initialization even if subsequent tool discovery fails.
    pub(crate) server_capabilities: Arc<StdMutex<Option<serde_json::Value>>>,
    pub(crate) tool_catalog_cache_context: Option<McpToolCatalogCacheContext>,
    pub(crate) startup_complete: Arc<AtomicBool>,
    pub(crate) cancel_token: CancellationToken,
}

impl AsyncManagedClient {
    // Keep this constructor flat so the startup inputs remain readable at the
    // single call site instead of introducing a one-off params wrapper.
    #[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        server_name: String,
        server: EffectiveMcpServer,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        oauth_refresh_mode: McpOAuthRefreshMode,
        cancel_token: CancellationToken,
        tx_event: Option<Sender<Event>>,
        elicitation_requests: ElicitationRequestManager,
        tool_catalog_cache_context: Option<McpToolCatalogCacheContext>,
        runtime_context: McpRuntimeContext,
        runtime_auth_provider: Option<SharedAuthProvider>,
        client_elicitation_capability: ElicitationCapability,
        client_mcp_extensions: ClientMcpExtensions,
        auth_changes: Option<watch::Receiver<AuthChangeState>>,
        protocol_mode: McpProtocolMode,
        catalog_item_limit: usize,
    ) -> Self {
        let startup_complete = Arc::new(AtomicBool::new(false));
        let server_capabilities = Arc::new(StdMutex::new(None));
        let startup = Arc::new(ManagedClientStartup {
            originator: AuthStorageOriginator::current(),
            server_name,
            server,
            store_mode,
            keyring_backend_kind,
            oauth_refresh_mode,
            tx_event,
            elicitation_requests,
            tool_catalog_cache_context: tool_catalog_cache_context.clone(),
            runtime_context,
            runtime_auth_provider,
            client_elicitation_capability,
            client_mcp_extensions,
            auth_changes,
            protocol_mode,
            catalog_item_limit,
            cancel_token: cancel_token.clone(),
            startup_complete: Arc::clone(&startup_complete),
            server_capabilities: Arc::clone(&server_capabilities),
        });
        let client = startup.start();
        Self {
            client,
            server_capabilities,
            tool_catalog_cache_context,
            startup_complete,
            cancel_token,
        }
    }

    pub(crate) async fn client(&self) -> Result<ManagedClient, StartupOutcomeError> {
        self.client.clone().await
    }

    /// Returns the current ready client, including its tool catalog and metadata,
    /// without waiting for startup.
    pub(crate) fn ready_client(&self) -> Option<ManagedClient> {
        self.client
            .peek()
            .and_then(|result| result.as_ref().ok())
            .cloned()
    }

    pub(crate) fn ready_transport(&self) -> Option<Arc<RmcpClient>> {
        self.ready_client().map(|client| client.client)
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel_token.cancel();
        match self.client().await {
            Ok(client) => client.client.shutdown().await,
            Err(StartupOutcomeError::Cancelled) => {}
            Err(error) => {
                warn!("failed to initialize MCP client during shutdown: {error:#}");
            }
        }
    }

    pub(crate) fn has_cached_tools(&self) -> bool {
        self.tool_catalog_cache_context
            .as_ref()
            .is_some_and(McpToolCatalogCacheContext::has_tools)
    }

    pub(crate) fn cached_tools(&self) -> Option<Vec<ToolInfo>> {
        self.cached_tools_or(/*fallback*/ None)
    }

    pub(crate) fn cached_tools_or(&self, fallback: Option<Vec<ToolInfo>>) -> Option<Vec<ToolInfo>> {
        self.tool_catalog_cache_context
            .as_ref()
            .and_then(|cache| cache.current_tools_or(fallback))
    }

    pub(crate) async fn listed_tools(&self) -> Result<Vec<ToolInfo>, StartupOutcomeError> {
        if !self.startup_complete.load(Ordering::Acquire)
            && let Some(startup_tools) = self.cached_tools()
        {
            Ok(startup_tools)
        } else {
            Ok(self.client().await?.listed_tools().await)
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum StartupOutcomeError {
    #[error("MCP startup cancelled")]
    Cancelled,
    // We can't store the original error here because anyhow::Error doesn't implement
    // `Clone`.
    #[error("MCP startup failed: {error}")]
    Failed {
        error: String,
        is_authentication_required: bool,
    },
}

impl StartupOutcomeError {
    pub(crate) fn is_authentication_required(&self) -> bool {
        match self {
            Self::Cancelled => false,
            Self::Failed {
                error,
                is_authentication_required,
            } => *is_authentication_required || error.contains("Auth required"),
        }
    }
}

impl From<anyhow::Error> for StartupOutcomeError {
    fn from(error: anyhow::Error) -> Self {
        let is_authentication_required = is_authentication_required_error(&error);
        Self::Failed {
            error: format!("{error:#}"),
            is_authentication_required,
        }
    }
}

#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
pub(crate) async fn list_tools_for_client_uncached(
    server_name: &str,
    client: &Arc<RmcpClient>,
    timeout: Option<Duration>,
    catalog_item_limit: usize,
    server_instructions: Option<&str>,
) -> Result<Vec<ToolInfo>> {
    let fetch_start = Instant::now();
    let tools = collect_paginated_with_limit("tools/list", timeout, catalog_item_limit, |params| {
        let client = Arc::clone(client);
        async move {
            let response = client.list_tools(params, timeout).await?;
            Ok((response.tools, response.next_cursor))
        }
    })
    .await?
    .into_iter()
    .map(|tool| tool_info_from_listed_tool(server_name, server_instructions, tool))
    .collect();
    emit_duration(
        MCP_TOOLS_FETCH_UNCACHED_DURATION_METRIC,
        fetch_start.elapsed(),
        &[],
    );
    Ok(tools)
}

/// Keeps the raw tool name and uses the MCP server name and instructions for the namespace.
fn tool_info_from_listed_tool(
    server_name: &str,
    server_instructions: Option<&str>,
    tool: rmcp::model::Tool,
) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_owned(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: tool.name.to_string(),
        callable_namespace: server_name.to_string(),
        namespace_description: server_instructions.map(str::to_string),
        tool,
    }
}

fn resolve_bearer_token(
    server_name: &str,
    bearer_token_env_var: Option<&str>,
) -> Result<Option<String>> {
    let Some(env_var) = bearer_token_env_var else {
        return Ok(None);
    };

    match env::var(env_var) {
        Ok(value) => {
            if value.is_empty() {
                Err(anyhow!(
                    "Environment variable {env_var} for MCP server '{server_name}' is empty"
                ))
            } else {
                Ok(Some(value))
            }
        }
        Err(env::VarError::NotPresent) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' is not set"
        )),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "Environment variable {env_var} for MCP server '{server_name}' contains invalid Unicode"
        )),
    }
}

fn validate_mcp_server_name(server_name: &str) -> Result<()> {
    let re = regex_lite::Regex::new(r"^[a-zA-Z0-9_:@/.-]+$")?;
    if !re.is_match(server_name) {
        return Err(anyhow!(
            "Invalid MCP server name '{server_name}': must match pattern {pattern}",
            pattern = re.as_str()
        ));
    }
    Ok(())
}

#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
async fn start_server_task(
    server_name: String,
    client: Arc<RmcpClient>,
    params: StartServerTaskParams,
) -> Result<ManagedClient, StartupOutcomeError> {
    let StartServerTaskParams {
        startup_timeout,
        tx_event,
        elicitation_requests,
        tool_catalog_cache_context,
        tool_catalog_fetch_ticket,
        client_elicitation_capability,
        client_mcp_extensions,
        auth_changes,
        catalog_item_limit,
        server_capabilities,
    } = params;
    let send_elicitation = elicitation_requests.make_sender(server_name.clone(), tx_event);
    let mut params =
        mcp_initialize_request_params(client_elicitation_capability, client_mcp_extensions);
    if auth_changes.is_some() {
        params
            .capabilities
            .experimental
            .get_or_insert_default()
            .insert(
                crate::auth_changes::CAPABILITY.to_string(),
                Default::default(),
            );
    }

    let started_at = Instant::now();
    let initialize_result = client
        .initialize(params, startup_timeout, send_elicitation)
        .await;
    record_protocol_discovery_metrics(client.protocol_mode(), started_at, &initialize_result);
    let initialize_result = initialize_result.map_err(StartupOutcomeError::from)?;
    *server_capabilities
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(serde_json::json!(initialize_result.capabilities));

    let auth_change_notifications = crate::auth_changes::start(
        Arc::clone(&client),
        &initialize_result.capabilities,
        auth_changes,
    )
    .await
    .map_err(StartupOutcomeError::from)?;

    let server_disables_tool_catalog_cache = initialize_result
        .capabilities
        .experimental
        .as_ref()
        .and_then(|experimental| experimental.get(MCP_TOOL_CATALOG_CACHE_CAPABILITY))
        .and_then(|capability| capability.get(MCP_TOOL_CATALOG_CACHEABLE_PROPERTY))
        .and_then(serde_json::Value::as_bool)
        == Some(false);
    if server_disables_tool_catalog_cache
        && let Some(cache_context) = tool_catalog_cache_context.as_ref()
    {
        cache_context.disable();
    }
    let list_start = Instant::now();
    let server_info =
        mcp_server_info_from_implementation(&server_name, initialize_result.server_info);
    let tools = list_tools_for_client_uncached(
        &server_name,
        &client,
        startup_timeout,
        catalog_item_limit,
        initialize_result.instructions.as_deref(),
    )
    .await
    .map_err(StartupOutcomeError::from)?;
    let client_tools: Arc<[ToolInfo]> = tools.into();
    if let (Some(cache_context), Some(fetch_ticket)) = (
        tool_catalog_cache_context.as_ref(),
        tool_catalog_fetch_ticket,
    ) {
        cache_context.publish_if_newest(fetch_ticket, &client_tools);
    }
    if tool_catalog_cache_context.is_some() {
        emit_duration(
            MCP_TOOLS_LIST_DURATION_METRIC,
            list_start.elapsed(),
            &[("cache", "miss")],
        );
    }
    let managed = ManagedClient {
        _auth_change_notifications: auth_change_notifications,
        client: Arc::clone(&client),
        server_info,
        tool_catalog: Arc::new(ClientToolCatalog::new(client_tools)),
        tool_timeout: None,
    };

    Ok(managed)
}

fn record_protocol_discovery_metrics(
    mode: McpProtocolMode,
    started_at: Instant,
    result: &Result<ServerPeerInfo>,
) {
    let Some(metrics) = codex_otel::global() else {
        return;
    };

    let mode = match mode {
        McpProtocolMode::Legacy => "legacy",
        McpProtocolMode::V20260728 => "auto",
    };
    let outcome = match result {
        Ok(result) if result.protocol_version == ProtocolVersion::V_2026_07_28 => "modern",
        Ok(_) => "legacy",
        Err(_) => "failure",
    };
    let tags = [("mode", mode), ("outcome", outcome)];
    let _ = metrics.counter("codex.mcp.protocol_discovery", /*inc*/ 1, &tags);
    let _ = metrics.record_duration(
        "codex.mcp.protocol_discovery.duration_ms",
        started_at.elapsed(),
        &tags,
    );
}

pub(crate) fn mcp_initialize_request_params(
    client_elicitation_capability: ElicitationCapability,
    client_mcp_extensions: ClientMcpExtensions,
) -> InitializeRequestParams {
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation = Some(client_elicitation_capability);
    let extensions = client_mcp_extensions
        .iter()
        .filter_map(|(id, settings)| {
            settings
                .as_object()
                .cloned()
                .map(|settings| (id.to_string(), settings))
        })
        .collect::<BTreeMap<_, _>>();
    if !extensions.is_empty() {
        capabilities.extensions = Some(extensions);
    }
    InitializeRequestParams::new(
        capabilities,
        Implementation::new("codex-mcp-client", env!("CARGO_PKG_VERSION")).with_title("Codex"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

fn mcp_server_info_from_implementation(
    server_name: &str,
    server_info: Option<Implementation>,
) -> McpServerInfo {
    let server_info = server_info.unwrap_or_else(|| Implementation::new(server_name, ""));
    McpServerInfo {
        name: server_info.name,
        title: server_info.title,
        version: server_info.version,
        description: server_info.description,
        icons: server_info.icons.map(|icons| {
            icons
                .into_iter()
                .filter_map(|icon| serde_json::to_value(icon).ok())
                .collect()
        }),
        website_url: server_info.website_url,
    }
}

struct StartServerTaskParams {
    server_capabilities: Arc<StdMutex<Option<serde_json::Value>>>,
    startup_timeout: Option<Duration>, // TODO: cancel_token should handle this.
    tx_event: Option<Sender<Event>>,
    elicitation_requests: ElicitationRequestManager,
    tool_catalog_cache_context: Option<McpToolCatalogCacheContext>,
    tool_catalog_fetch_ticket: Option<McpToolCatalogFetchTicket>,
    client_elicitation_capability: ElicitationCapability,
    client_mcp_extensions: ClientMcpExtensions,
    auth_changes: Option<watch::Receiver<AuthChangeState>>,
    catalog_item_limit: usize,
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace", skip_all, fields(server_name = %server_name))]
pub(crate) async fn make_rmcp_client(
    server_name: &str,
    server: EffectiveMcpServer,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    oauth_refresh_mode: McpOAuthRefreshMode,
    runtime_context: McpRuntimeContext,
    runtime_auth_provider: Option<SharedAuthProvider>,
    protocol_mode: McpProtocolMode,
) -> Result<RmcpClient, StartupOutcomeError> {
    let config = server.config().clone();
    if matches!(config.auth, McpServerAuth::EmaAuth) {
        return Err(StartupOutcomeError::from(anyhow!(
            "EMA MCP connections are not enabled in this version"
        )));
    }
    let oauth_credential_name = config.oauth_credential_name(server_name);
    let McpServerConfig { transport, .. } = config;

    match transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            let command_os: OsString = command.into();
            let args_os: Vec<OsString> = args.into_iter().map(Into::into).collect();
            let env_os = env.map(|env| {
                env.into_iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect::<HashMap<_, _>>()
            });
            let launcher = Arc::new(LocalStdioServerLauncher::new(
                runtime_context.local_process_cwd(),
            )) as Arc<dyn StdioServerLauncher>;

            let cwd = cwd.map(codex_utils_path_uri::LegacyAppPathString::into_string);
            RmcpClient::new_stdio_client_with_protocol_mode(
                command_os,
                args_os,
                env_os,
                &env_vars,
                cwd,
                launcher,
                protocol_mode,
            )
            .await
            .map_err(|err| StartupOutcomeError::from(anyhow!(err)))
        }
        McpServerTransportConfig::StreamableHttp {
            url,
            http_headers,
            env_http_headers,
            bearer_token_env_var,
            http_headers_helper: _,
        } => {
            let http_client = runtime_context
                .http_client_for_server(server.config())
                .map_err(|error| StartupOutcomeError::from(anyhow!(error)))?;
            let http_client = maybe_with_openai_docs_source_attribution(&url, http_client);
            let resolved_bearer_token =
                resolve_bearer_token(server_name, bearer_token_env_var.as_deref())
                    .map_err(StartupOutcomeError::from)?
                    .map(StreamableHttpBearerToken::Resolved);
            let redirect_mode = StreamableHttpRedirectMode::Legacy;
            RmcpClient::new_streamable_http_client_with_protocol_mode_and_redirect_mode(
                oauth_credential_name.as_ref(),
                &url,
                resolved_bearer_token,
                http_headers,
                env_http_headers,
                server.config().oauth.clone(),
                store_mode,
                keyring_backend_kind,
                http_client,
                runtime_auth_provider,
                protocol_mode,
                redirect_mode,
                oauth_refresh_mode,
            )
            .await
            .map_err(StartupOutcomeError::from)
        }
    }
}

//! Runtime support for Model Context Protocol (MCP) servers.
//!
//! This module contains the thread-owned MCP runtime and data that describes the
//! environment in which MCP servers execute. Transport startup lives in
//! [`crate::rmcp_client`] and connection-set behavior lives in
//! [`crate::connection_manager`].

mod status;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_channel::Sender;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::HttpClient;
use codex_exec_server::RouteAwareHttpClient;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::with_http_headers_helper;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::RequestId;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::McpConfig;
use crate::binding::McpBinding;
use crate::binding::PreparedMcpCall;
use crate::connection_manager::BindingCatalogRevision;
use crate::connection_manager::McpConnectionSet;
use crate::elicitation::ElicitationLifecycle;
use crate::elicitation::ElicitationRequestRouter;
use crate::server::EffectiveMcpServer;
use crate::tool_catalog_cache::McpToolCatalogCache;
use crate::tools::ToolInfo;

/// Controls when one task starts its eligible MCP servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpStartupPolicy {
    /// Start configured servers when their task's MCP runtime is published.
    Eager,
    /// Start servers with cached tool definitions on first use.
    LazyWhenCached,
}

/// Configuration and owning-thread state needed to materialize an MCP runtime.
pub struct McpRuntimeInput {
    pub startup_policy: McpStartupPolicy,
    pub config: Arc<McpConfig>,
    pub mcp_servers: HashMap<String, EffectiveMcpServer>,
    pub submit_id: String,
    /// Unbounded sources receive abandonment notifications; bounded sources retain request-only behavior.
    pub tx_event: Option<Sender<Event>>,
    pub startup_cancellation_token: CancellationToken,
    pub runtime_context: McpRuntimeContext,
    pub tool_catalog_cache: McpToolCatalogCache,
    pub client_mcp_extensions: ClientMcpExtensions,
    pub auth: Option<CodexAuth>,
    pub auth_manager: Option<Arc<AuthManager>>,
    pub elicitation_lifecycle: Option<ElicitationLifecycle>,
}

/// Owns all mutable MCP state for one Codex thread.
///
/// Publication replaces the latest state atomically. Existing bindings retain
/// their exact connections and configuration for as long as they are needed.
pub struct McpRuntime {
    current: ArcSwap<PublishedMcpRuntime>,
    reconnect_pending: AtomicBool,
    elicitation_router: ElicitationRequestRouter,
}

struct PublishedMcpRuntime {
    connections: Arc<McpConnectionSet>,
    config: Option<Arc<McpConfig>>,
    auth: Option<CodexAuth>,
    auth_token: Option<String>,
    environment_selections: Arc<[TurnEnvironmentSelection]>,
    ready_environments: HashMap<String, Arc<Environment>>,
    cached_binding: Mutex<Option<CachedMcpBinding>>,
}

impl PublishedMcpRuntime {
    // Shared by dirty detection and cache-key publication so their auth rules agree.
    fn auth_matches(&self, auth: Option<&CodexAuth>) -> bool {
        match (self.auth.as_ref(), auth) {
            (Some(previous), Some(latest)) => {
                previous == latest
                    && previous.get_account_id() == latest.get_account_id()
                    && previous.get_chatgpt_user_id() == latest.get_chatgpt_user_id()
                    && previous.is_fedramp_account() == latest.is_fedramp_account()
                    && self.auth_token == latest.get_token().ok()
            }
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
    }
}

struct CachedMcpBinding {
    catalog_revisions: HashMap<String, BindingCatalogRevision>,
    // Reuse a frozen binding while a model step or caller still needs it.
    binding: Weak<McpBinding>,
}

struct McpReconnectGuard<'a> {
    pending: &'a AtomicBool,
    claimed: bool,
}

impl Drop for McpReconnectGuard<'_> {
    fn drop(&mut self) {
        if self.claimed {
            self.pending.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
pub(crate) struct McpPublicationGate {
    published: Option<watch::Receiver<bool>>,
}

impl McpPublicationGate {
    fn pending() -> (watch::Sender<bool>, Self) {
        let (publish, published) = watch::channel(false);
        (
            publish,
            Self {
                published: Some(published),
            },
        )
    }

    pub(crate) fn already_published() -> Self {
        Self { published: None }
    }

    pub(crate) async fn wait(mut self) -> bool {
        let Some(published) = self.published.as_mut() else {
            return true;
        };
        loop {
            if *published.borrow() {
                return true;
            }
            if published.changed().await.is_err() {
                return false;
            }
        }
    }
}

impl McpRuntime {
    /// Creates a runtime with no configured servers.
    ///
    /// This is useful while constructing a thread that must publish a stable
    /// runtime handle before its full MCP inputs are available.
    pub fn empty(prefix_mcp_tool_names: bool) -> Self {
        Self {
            current: ArcSwap::from_pointee(PublishedMcpRuntime {
                connections: Arc::new(McpConnectionSet::empty(prefix_mcp_tool_names)),
                config: None,
                auth: None,
                auth_token: None,
                environment_selections: Arc::default(),
                ready_environments: HashMap::new(),
                cached_binding: Mutex::new(None),
            }),
            reconnect_pending: AtomicBool::new(false),
            elicitation_router: ElicitationRequestRouter::default(),
        }
    }

    pub async fn new(input: McpRuntimeInput) -> Self {
        let runtime = Self::empty(input.config.prefix_mcp_tool_names);
        runtime.replace(input).await;
        runtime
    }

    /// Reconciles configured servers and publishes their immutable runtime snapshot.
    pub async fn replace(&self, input: McpRuntimeInput) {
        let current = self.current.load_full();
        let mut reconnect = McpReconnectGuard {
            pending: &self.reconnect_pending,
            claimed: self.reconnect_pending.swap(false, Ordering::AcqRel),
        };
        self.publish(
            input,
            (!reconnect.claimed).then_some(current.connections.as_ref()),
        )
        .await;
        reconnect.claimed = false;
    }

    async fn publish(&self, input: McpRuntimeInput, previous: Option<&McpConnectionSet>) {
        let (publish, publication_gate) = McpPublicationGate::pending();
        let config = Arc::clone(&input.config);
        let auth = input.auth.clone();
        let auth_token = auth.as_ref().and_then(|auth| auth.get_token().ok());
        let environment_selections = Arc::clone(&input.runtime_context.environment_selections);
        let ready_environments = input.runtime_context.ready_environments.clone();
        let connections = Arc::new(
            McpConnectionSet::new(
                previous,
                publication_gate,
                input,
                self.elicitation_router.clone(),
            )
            .await,
        );
        self.current.store(Arc::new(PublishedMcpRuntime {
            connections,
            config: Some(config),
            auth,
            auth_token,
            environment_selections,
            ready_environments,
            cached_binding: Mutex::new(None),
        }));
        let _ = publish.send(true);
    }

    /// Ensures the next refresh creates fresh connections for every configured server.
    pub fn reconnect_on_next_refresh(&self) {
        self.reconnect_pending.store(true, Ordering::Release);
    }

    /// Captures the latest published configuration and live client handles.
    pub async fn current_binding(&self) -> Option<Arc<McpBinding>> {
        self.current_binding_with_requirements(&[]).await
    }

    /// Captures one runtime, waiting for explicitly required servers.
    pub async fn current_binding_with_requirements(
        &self,
        required_servers: &[String],
    ) -> Option<Arc<McpBinding>> {
        let current = self.current.load_full();
        current
            .connections
            .record_startup_readiness("model_binding", required_servers);
        Self::binding_from_published_runtime(current, required_servers).await
    }

    async fn binding_from_published_runtime(
        current: Arc<PublishedMcpRuntime>,
        required_servers: &[String],
    ) -> Option<Arc<McpBinding>> {
        let config = Arc::clone(current.config.as_ref()?);
        let stable_catalog_revisions = current
            .connections
            .stable_catalog_revisions(required_servers)
            .await;
        if let Some(catalog_revisions) = &stable_catalog_revisions {
            let cached = current
                .cached_binding
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cached.as_ref()
                && &cached.catalog_revisions == catalog_revisions
                && let Some(binding) = cached.binding.upgrade()
            {
                return Some(binding);
            }
        }

        let binding = Arc::new(
            current
                .connections
                .capture_binding_with_metadata(config, required_servers)
                .await,
        );
        if let Some(catalog_revisions) = stable_catalog_revisions
            && current
                .connections
                .stable_catalog_revisions(required_servers)
                .await
                .as_ref()
                == Some(&catalog_revisions)
        {
            let mut cached = current
                .cached_binding
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cached.as_ref()
                && cached.catalog_revisions == catalog_revisions
                && let Some(binding) = cached.binding.upgrade()
            {
                return Some(binding);
            }
            *cached = Some(CachedMcpBinding {
                catalog_revisions,
                binding: Arc::downgrade(&binding),
            });
        }
        Some(binding)
    }

    /// Returns whether the published snapshot still belongs to the current credentials.
    pub fn current_auth_matches(&self, auth: Option<&CodexAuth>) -> bool {
        self.current.load().auth_matches(auth)
    }

    /// Detects newly saved credentials for servers whose startup failed authentication.
    pub async fn updated_oauth_credentials_after_auth_failure(&self) -> Vec<String> {
        let current = self.current.load_full();
        let Some(config) = current.config.as_ref() else {
            return Vec::new();
        };
        current
            .connections
            .updated_oauth_credentials_after_auth_failure(config)
            .await
    }

    /// Checks the current generation before retrying servers detected outside the refresh gate.
    pub async fn has_authentication_failed_servers(&self, server_names: &[String]) -> bool {
        self.current
            .load_full()
            .connections
            .authentication_failed_servers()
            .await
            .into_iter()
            .any(|server_name| server_names.contains(&server_name))
    }

    /// Waits for the selected server without capturing an execution binding.
    pub async fn wait_for_server_startup(&self, server: &str) {
        self.current
            .load_full()
            .connections
            .wait_for_server_startup(server)
            .await;
    }

    /// Prepares one advertised tool against this publication's current client and policy.
    /// Its model-visible name stays fixed while execution metadata comes from the live catalog.
    pub async fn prepare_call(&self, advertised_tool: &ToolInfo) -> Option<PreparedMcpCall> {
        let current = self.current.load_full();
        current
            .connections
            .prepare_call_for_tool(Arc::clone(current.config.as_ref()?), advertised_tool)
            .await
    }

    /// Captures the current runtime after its selected server has finished startup.
    pub async fn current_binding_for_call(&self, server: &str) -> Option<Arc<McpBinding>> {
        let current = self.current.load_full();
        current.config.as_ref()?;
        if !current.connections.wait_for_server_startup(server).await {
            return None;
        }
        Self::binding_from_published_runtime(current, /*required_servers*/ &[]).await
    }

    /// Returns the latest published configuration without waiting for clients.
    pub fn current_config(&self) -> Option<Arc<McpConfig>> {
        self.current.load().config.clone()
    }

    /// Whether this publication uses the same selections and currently ready environment handles.
    pub fn current_environments_match(
        &self,
        selections: &[TurnEnvironmentSelection],
        ready_environments: &HashMap<String, Arc<Environment>>,
    ) -> bool {
        let current = self.current.load();
        current.config.is_some()
            && current.environment_selections.as_ref() == selections
            && current.ready_environments.len() == ready_environments.len()
            && ready_environments.iter().all(|(id, environment)| {
                current
                    .ready_environments
                    .get(id)
                    .is_some_and(|published| Arc::ptr_eq(published, environment))
            })
    }

    pub fn elicitations_auto_deny(&self) -> bool {
        self.elicitation_router.auto_deny()
    }

    pub fn set_elicitations_auto_deny(&self, auto_deny: bool) {
        self.elicitation_router.set_auto_deny(auto_deny);
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.elicitation_router
            .resolve(server_name, id, response)
            .await
    }

    /// Lists the latest known tools for non-model discovery surfaces.
    ///
    /// Unlike [`Self::current_binding`], this may return cached tools while their
    /// client reconnects because callers only inspect tool metadata.
    pub async fn latest_list_all_tools(&self) -> Vec<ToolInfo> {
        self.latest_connections().list_all_tools().await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn latest_call_tool(
        &self,
        server: &str,
        tool: &str,
        environment_id: Option<&str>,
        arguments: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
        requested_timeout: Option<Duration>,
        wait_for_server: bool,
    ) -> anyhow::Result<CallToolResult> {
        self.latest_connections()
            .call_tool(
                server,
                tool,
                environment_id,
                arguments,
                meta,
                requested_timeout,
                wait_for_server,
            )
            .await
    }

    pub async fn latest_read_resource(
        &self,
        server: &str,
        params: ReadResourceRequestParams,
    ) -> anyhow::Result<ReadResourceResult> {
        self.latest_connections()
            .read_resource(server, params)
            .await
    }

    pub async fn latest_wait_for_server_ready(&self, server: &str, timeout: Duration) -> bool {
        self.latest_connections()
            .wait_for_server_ready(server, timeout)
            .await
    }

    pub async fn validate_required_servers(&self) -> anyhow::Result<()> {
        self.latest_connections().validate_required_servers().await
    }

    pub fn cancel_startup(&self) {
        self.current.load().connections.cancel_startup();
    }

    /// Observes matching published registrations without starting or reconnecting servers.
    pub async fn connection_statuses(
        &self,
        config: &McpConfig,
    ) -> std::collections::HashMap<String, codex_protocol::mcp::McpServerConnectionStatus> {
        let current = self.current.load_full();
        let Some(published_config) = current.config.as_ref() else {
            return HashMap::new();
        };
        let mut statuses = current.connections.connection_statuses().await;
        statuses.retain(|name, _| {
            published_config
                .mcp_server_catalog
                .server(name)
                .is_some_and(|server| config.mcp_server_catalog.server(name) == Some(server))
        });
        statuses
    }

    pub(crate) fn latest_connections(&self) -> Arc<McpConnectionSet> {
        Arc::clone(&self.current.load().connections)
    }

    pub async fn shutdown(&self) {
        self.elicitation_router.close().await;
        self.latest_connections().shutdown().await;
    }
}

/// Runtime context used when resolving per-server MCP environments.
///
/// `McpConfig` describes what servers exist. This value carries the canonical
/// environment registry, captured thread selections, and the host-local cwd used by MCP processes.
#[derive(Clone)]
pub struct McpRuntimeContext {
    environment_manager: Arc<EnvironmentManager>,
    environment_selections: Arc<[TurnEnvironmentSelection]>,
    ready_environments: HashMap<String, Arc<Environment>>,
    local_process_cwd: PathBuf,
    local_http_client: Arc<dyn HttpClient>,
}

/// Applies the local HTTP headers helper configured for an MCP server.
///
/// Callers retain ownership of selecting the underlying HTTP transport. This
/// function centralizes the helper-specific policy checks and decoration used
/// by both MCP runtime startup and standalone OAuth login.
pub fn apply_http_headers_helper(
    client: Arc<dyn HttpClient>,
    config: &codex_config::McpServerConfig,
    local_process_cwd: PathBuf,
) -> Result<Arc<dyn HttpClient>, String> {
    let codex_config::McpServerTransportConfig::StreamableHttp {
        url,
        http_headers_helper: Some(command),
        ..
    } = &config.transport
    else {
        return Ok(client);
    };
    if !config.is_local_environment() {
        return Err("HTTP headers helpers can only run in the local environment".to_string());
    }
    with_http_headers_helper(client, url, command, local_process_cwd)
        .map_err(|error| error.to_string())
}

impl McpRuntimeContext {
    pub fn new(environment_manager: Arc<EnvironmentManager>, local_process_cwd: PathBuf) -> Self {
        let local_http_client = Arc::new(
            RouteAwareHttpClient::new(environment_manager.http_client_factory().clone())
                .with_tls_backend_fallback(),
        );
        Self {
            environment_manager,
            environment_selections: Arc::default(),
            ready_environments: HashMap::new(),
            local_process_cwd,
            local_http_client,
        }
    }

    /// Pins all captured selections and the concrete handles for environments already ready.
    pub fn with_selected_environments(
        mut self,
        environment_selections: Arc<[TurnEnvironmentSelection]>,
        ready_environments: HashMap<String, Arc<Environment>>,
    ) -> Self {
        self.environment_selections = environment_selections;
        self.ready_environments = ready_environments;
        self
    }

    pub(crate) fn local_process_cwd(&self) -> PathBuf {
        self.local_process_cwd.clone()
    }

    pub(crate) fn local_http_client(&self) -> Arc<dyn HttpClient> {
        Arc::clone(&self.local_http_client)
    }

    pub(crate) fn resolve_server_environment(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Option<Arc<Environment>>, String> {
        // Resolve `"local"` through the shared registry when available. Local
        // HTTP is the one current exception: it can use the ambient HTTP client
        // even when no local Environment is configured.
        if let Some(environment) = self
            .ready_environments
            .get(&config.environment_id)
            .cloned()
            .or_else(|| {
                self.environment_manager
                    .get_environment(&config.environment_id)
            })
        {
            return Ok(Some(environment));
        }

        if config.is_local_environment() {
            return match config.transport {
                codex_config::McpServerTransportConfig::Stdio { .. } => Err(format!(
                    "local stdio MCP server `{server_name}` requires a local environment"
                )),
                codex_config::McpServerTransportConfig::StreamableHttp { .. } => Ok(None),
            };
        }

        Err(format!(
            "MCP server `{server_name}` references unknown environment id `{}`",
            config.environment_id
        ))
    }

    /// Resolves local MCP's specialized HTTP capability or the selected remote capability.
    pub fn resolve_http_client(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Arc<dyn HttpClient>, String> {
        let environment = self.resolve_server_environment(server_name, config)?;
        self.http_client_for_server(config, environment.as_ref())
    }

    pub(crate) fn http_client_for_server(
        &self,
        config: &codex_config::McpServerConfig,
        environment: Option<&Arc<Environment>>,
    ) -> Result<Arc<dyn HttpClient>, String> {
        let client = match environment {
            Some(environment) if environment.is_remote() => environment.get_http_client(),
            Some(_) | None => self.local_http_client(),
        };
        apply_http_headers_helper(client, config, self.local_process_cwd())
    }
}

pub(crate) fn emit_duration(metric: &str, duration: Duration, tags: &[(&str, &str)]) {
    if let Some(metrics) = codex_otel::global() {
        let _ = metrics.record_duration(metric, duration, tags);
    }
}

//! Thread MCP runtime projection and publication.
//!
//! This module owns the small correctness boundary between immutable session
//! inputs and the mutable [`codex_mcp::McpRuntime`]. Background scheduling
//! belongs elsewhere.

use super::session::SessionConfiguration;
use super::*;
use codex_config::McpServerTransportConfig;
use codex_mcp::McpServerRegistration;
use codex_mcp::McpStartupPolicy;
use codex_mcp::PreparedMcpCall;
use codex_mcp::ToolInfo;
use codex_otel::auth_storage::AuthStorageOriginator;
use std::collections::HashSet;

pub(super) struct McpDesiredState {
    pub(super) config: Arc<Config>,
    pub(super) auth: Option<CodexAuth>,
    pub(super) submit_id: String,
    pub(super) originator: String,
    pub(super) session_source: SessionSource,
    pub(super) environments: TurnEnvironmentSnapshot,
    pub(super) local_process_cwd: PathBuf,
}

impl Session {
    pub(super) fn mcp_inputs_differ(
        &self,
        current: &SessionConfiguration,
        next: &SessionConfiguration,
    ) -> bool {
        current.cwd() != next.cwd()
    }

    /// Waits on this session's refreshed server before tool execution is admitted.
    pub(crate) async fn wait_for_mcp_server(self: &Arc<Self>, server: &str) {
        self.refresh_mcp_if_dirty().await;
        self.services
            .mcp_runtime
            .wait_for_server_startup(server)
            .await;
    }

    /// Captures this session's current MCP client and catalog for one tool call.
    pub(crate) async fn prepare_mcp_call(
        self: &Arc<Self>,
        advertised_tool: &ToolInfo,
    ) -> Option<PreparedMcpCall> {
        self.refresh_mcp_if_dirty().await;
        self.services
            .mcp_runtime
            .prepare_call(advertised_tool)
            .await
    }

    pub(super) async fn latest_mcp_desired_state(
        &self,
        auth: Option<CodexAuth>,
        environments: TurnEnvironmentSnapshot,
    ) -> McpDesiredState {
        let session_configuration = self.state.lock().await.session_configuration.clone();
        let cwd = environments
            .primary()
            .and_then(|environment| environment.cwd().to_abs_path().ok())
            .unwrap_or_else(|| session_configuration.cwd().clone());
        let config = self.build_per_turn_config(
            &session_configuration,
            cwd,
            environments.primary_workspace_roots(),
        );
        let local_process_cwd = environments
            .local_environment_cwd()
            .unwrap_or_else(|| session_configuration.cwd().clone())
            .to_path_buf();

        McpDesiredState {
            config: Arc::new(config),
            auth,
            submit_id: self.next_internal_sub_id(),
            originator: session_configuration.originator,
            session_source: session_configuration.session_source,
            environments,
            local_process_cwd,
        }
    }

    pub(super) async fn install_initial_mcp_runtime(
        self: &Arc<Self>,
        session_configuration: &SessionConfiguration,
        auth: Option<CodexAuth>,
        mcp_config: McpConfig,
        resolved_environments: &TurnEnvironmentSnapshot,
        mcp_runtime_cwd: PathBuf,
    ) -> anyhow::Result<()> {
        let cwd = AbsolutePathBuf::from_absolute_path(mcp_runtime_cwd)
            .unwrap_or_else(|_| session_configuration.cwd().clone());
        let config = self.build_per_turn_config(
            session_configuration,
            cwd,
            resolved_environments.primary_workspace_roots(),
        );
        let local_process_cwd = resolved_environments
            .local_environment_cwd()
            .unwrap_or_else(|| session_configuration.cwd().clone())
            .to_path_buf();
        let desired = McpDesiredState {
            config: Arc::new(config),
            auth,
            submit_id: INITIAL_SUBMIT_ID.to_owned(),
            originator: session_configuration.originator.clone(),
            session_source: session_configuration.session_source.clone(),
            environments: resolved_environments.clone(),
            local_process_cwd,
        };
        self.publish_mcp_runtime(&desired, mcp_config)
            .instrument(info_span!(
                "session_init.mcp_manager_init",
                otel.name = "session_init.mcp_manager_init",
            ))
            .await;

        self.services.mcp_runtime.validate_required_servers().await
    }

    /// Adds effective executor-owned configuration from this exact thread snapshot.
    pub(super) fn project_selected_environment_mcp_servers<'a>(
        &'a self,
        environments: &'a TurnEnvironmentSnapshot,
        mut config: McpConfig,
    ) -> BoxFuture<'a, McpConfig> {
        Box::pin(async move {
            if self.isolation == codex_extension_api::SessionIsolation::Isolated {
                return config;
            }

            let mut catalog = None;
            let mut registered = HashSet::new();
            for selected in environments.turn_environments() {
                let environment = &selected.environment;
                if !environment.is_remote() {
                    continue;
                }

                let environment_id = &selected.selection.environment_id;
                let discovery = environment
                    .discover_http_mcp_servers(selected.cwd().clone())
                    .await;
                let outcome = if discovery.is_ok() {
                    "success"
                } else {
                    "error"
                };
                // Count completed discovery attempts, including refreshes, before host policy
                // or MCP startup determines whether the server's tools become available.
                self.services.session_telemetry.counter(
                    "codex.mcp.executor_discovery",
                    /*inc*/ 1,
                    &[("outcome", outcome)],
                );
                let servers = match discovery {
                    Ok(servers) => servers,
                    Err(error) => {
                        tracing::warn!(
                            environment_id,
                            %error,
                            "failed to discover executor-local MCP servers"
                        );
                        continue;
                    }
                };
                for (name, mut server) in servers {
                    let outcome = if server.enabled {
                        "found"
                    } else {
                        "unavailable"
                    };
                    self.services.session_telemetry.counter(
                        "codex.mcp.executor_discovery.server",
                        /*inc*/ 1,
                        &[("server_name", name.as_str()), ("outcome", outcome)],
                    );
                    if !server.is_local_environment()
                        || config.mcp_server_catalog.server(&name).is_some()
                        || registered.contains(&name)
                    {
                        continue;
                    }
                    let McpServerTransportConfig::StreamableHttp {
                        env_http_headers,
                        http_headers_helper,
                        ..
                    } = &server.transport
                    else {
                        continue;
                    };
                    if http_headers_helper.is_some()
                        || env_http_headers
                            .as_ref()
                            .is_some_and(|headers| !headers.is_empty())
                    {
                        tracing::warn!(
                            environment_id,
                            server = name,
                            "executor-local HTTP header helpers are not supported"
                        );
                        continue;
                    }
                    server.environment_id = environment_id.clone();
                    registered.insert(name.clone());
                    catalog
                        .get_or_insert_with(|| config.mcp_server_catalog.to_builder())
                        .register(McpServerRegistration::from_executor_config(name, server));
                }
            }

            if let Some(catalog) = catalog {
                let selections = environments.all_selections();
                config.mcp_server_catalog =
                    catalog.build_with_environment_authority(|environment_id| {
                        crate::mcp::environment_authority(&selections, environment_id)
                    });
            }
            config
        })
    }

    #[tracing::instrument(name = "mcp.runtime.refresh", skip_all)]
    pub(super) async fn publish_mcp_runtime(
        &self,
        desired: &McpDesiredState,
        mcp_config: McpConfig,
    ) {
        let mcp_config = self
            .project_selected_environment_mcp_servers(&desired.environments, mcp_config)
            .await;
        let input = self.build_mcp_runtime_input(desired, mcp_config);
        AuthStorageOriginator::from_client_name(&desired.originator)
            .scope(self.services.mcp_runtime.replace(input))
            .await;
    }

    pub(super) fn build_mcp_runtime_input(
        &self,
        desired: &McpDesiredState,
        mut config: McpConfig,
    ) -> McpRuntimeInput {
        let auth = desired.auth.clone();
        config.environment_cwds = desired
            .environments
            .turn_environments()
            .map(|environment| {
                (
                    environment.selection.environment_id.clone(),
                    environment.cwd().clone(),
                )
            })
            .collect();
        config
            .environment_cwds
            .entry(codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string())
            .or_insert_with(|| PathUri::from_abs_path(&desired.config.cwd));
        let mcp_servers = effective_mcp_servers(&config);
        let mcp_config = Arc::new(config);
        let runtime_context = McpRuntimeContext::new(
            self.services.turn_environments.environment_manager(),
            desired.local_process_cwd.clone(),
        )
        .with_selected_environments(
            desired.environments.all_selections().into(),
            desired.environments.ready_environment_handles(),
        );
        McpRuntimeInput {
            startup_policy: if matches!(desired.session_source, SessionSource::SubAgent(_)) {
                McpStartupPolicy::LazyWhenCached
            } else {
                McpStartupPolicy::Eager
            },
            config: mcp_config,
            mcp_servers,
            submit_id: desired.submit_id.clone(),
            tx_event: Some(self.get_tx_event()),
            startup_cancellation_token: CancellationToken::new(),
            runtime_context,
            tool_catalog_cache: self.services.mcp_manager.tool_catalog_cache(),
            client_mcp_extensions: self.services.client_mcp_extensions.for_mcp_servers(),
            auth,
            auth_manager: Some(Arc::clone(&self.services.auth_manager)),
            elicitation_lifecycle: Some(self.mcp_elicitation_lifecycle()),
        }
    }
}

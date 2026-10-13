use super::mcp_refresh::McpRefreshInvalidationGuard;
use super::*;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_otel::auth_storage::AuthStorageOriginator;

impl Session {
    pub(crate) async fn runtime_mcp_config(&self, config: &Config) -> McpConfig {
        self.runtime_mcp_config_and_context(config).await.0
    }

    pub(crate) async fn runtime_mcp_config_and_context(
        &self,
        config: &Config,
    ) -> (McpConfig, McpRuntimeContext) {
        let host_fallback_cwd = self.state.lock().await.session_configuration.cwd().clone();
        let environments = self.services.turn_environments.snapshot();
        let mcp_config = self.services.mcp_manager.runtime_config_for_step(config);
        let runtime_context = self.mcp_runtime_context(&environments, &host_fallback_cwd);
        (mcp_config, runtime_context)
    }

    pub(crate) async fn current_mcp_runtime_context(&self) -> McpRuntimeContext {
        let host_fallback_cwd = self.state.lock().await.session_configuration.cwd().clone();
        let environments = self.services.turn_environments.snapshot();
        self.mcp_runtime_context(&environments, &host_fallback_cwd)
    }

    fn mcp_runtime_context(
        &self,
        environments: &TurnEnvironmentSnapshot,
        host_fallback_cwd: &std::path::Path,
    ) -> McpRuntimeContext {
        let local_process_cwd = environments
            .primary_cwd()
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| host_fallback_cwd.to_path_buf());
        self.new_mcp_runtime_context(local_process_cwd)
    }

    pub(super) fn new_mcp_runtime_context(&self, local_process_cwd: PathBuf) -> McpRuntimeContext {
        McpRuntimeContext::new(
            self.services
                .turn_environments
                .environment_manager()
                .http_client_factory()
                .clone(),
            local_process_cwd,
        )
    }

    pub(crate) async fn runtime_mcp_servers(
        &self,
        config: &Config,
    ) -> HashMap<String, McpServerConfig> {
        codex_mcp::configured_mcp_servers(&self.runtime_mcp_config(config).await)
    }

    /// Publishes changed MCP state, waiting for any refresh already in progress.
    #[tracing::instrument(name = "mcp.runtime.refresh_if_dirty", skip_all)]
    pub(crate) async fn refresh_mcp_if_dirty(self: &Arc<Self>) {
        let Ok(_refresh) = self.mcp_refresh.acquire().await else {
            error!("MCP runtime refresh semaphore closed");
            return;
        };
        loop {
            // Compare and rebuild from current environments, not choices saved for a future turn.
            let environments = self.services.turn_environments.snapshot();
            let auth = self.services.auth_manager.auth_cached();
            if !self
                .services
                .mcp_runtime
                .current_auth_matches(auth.as_ref())
            {
                self.mark_mcp_runtime_dirty();
            }

            if !self.mcp_refresh.claim() {
                return;
            }
            let mut refresh_invalidation = McpRefreshInvalidationGuard {
                refresh: &self.mcp_refresh,
                published: false,
            };
            let auth = self.services.auth_manager.auth().await;
            let desired = self.latest_mcp_desired_state(auth, environments).await;
            let mcp_config = self
                .services
                .mcp_manager
                .runtime_config_for_step(&desired.config);
            self.publish_mcp_runtime(&desired, mcp_config).await;
            refresh_invalidation.published = true;
        }
    }

    pub(super) fn mark_mcp_runtime_dirty(&self) {
        self.mcp_refresh.invalidate();
    }

    #[tracing::instrument(
        name = "mcp.runtime.resolve_for_step",
        skip_all,
        fields(turn.id = %turn_context.sub_id)
    )]
    pub(crate) async fn mcp_runtime_for_step(
        self: &Arc<Self>,
        turn_context: &TurnContext,
        required_servers: &[String],
    ) -> Arc<codex_mcp::McpBinding> {
        let recovered_oauth_servers =
            AuthStorageOriginator::from_client_name(&turn_context.originator)
                .scope(
                    self.services
                        .mcp_runtime
                        .updated_oauth_credentials_after_auth_failure(),
                )
                .await;
        if !recovered_oauth_servers.is_empty()
            && let Ok(_refresh) = self.mcp_refresh.acquire().await
            && self
                .services
                .mcp_runtime
                .has_authentication_failed_servers(&recovered_oauth_servers)
                .await
        {
            self.mark_mcp_runtime_dirty();
        }
        self.refresh_mcp_if_dirty().await;
        let required_servers = required_servers
            .iter()
            .chain(&recovered_oauth_servers)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(binding) = self
            .services
            .mcp_runtime
            .current_binding_with_requirements(&required_servers)
            .await
        {
            return binding;
        }
        let config = Arc::new(self.runtime_mcp_config(&turn_context.config).await);
        Arc::new(codex_mcp::McpBinding::empty(config))
    }

    pub(crate) fn mcp_elicitation_lifecycle(&self) -> codex_mcp::ElicitationLifecycle {
        self.mcp_elicitation_lifecycle_handle
            .get_or_init(|| {
                let elicitations = self.services.elicitations.clone();
                codex_mcp::ElicitationLifecycle::new(move || elicitations.register())
            })
            .clone()
    }

    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        self.services
            .mcp_runtime
            .resolve_elicitation(server_name, id, response)
            .await
    }

    pub(crate) async fn refresh_mcp_servers_now(
        &self,
        added_servers: HashMap<String, McpServerConfig>,
    ) {
        let Ok(_refresh) = self.mcp_refresh.acquire().await else {
            error!("MCP runtime refresh semaphore closed");
            return;
        };
        let auth = self.services.auth_manager.auth().await;
        {
            let mut state = self.state.lock().await;
            let mut config = (*state.session_configuration.original_config_do_not_use).clone();
            for (name, server) in added_servers {
                config.mcp_servers.entry(name).or_insert(server);
            }
            state.session_configuration.original_config_do_not_use = Arc::new(config);
        }
        let environments = self.services.turn_environments.snapshot();
        let desired = self.latest_mcp_desired_state(auth, environments).await;
        let mcp_config = self
            .services
            .mcp_manager
            .runtime_config_for_step(&desired.config);
        self.publish_mcp_runtime(&desired, mcp_config).await;
    }

    pub(crate) fn cancel_mcp_startup(&self) {
        self.services.mcp_runtime.cancel_startup();
    }
}

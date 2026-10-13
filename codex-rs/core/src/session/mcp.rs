use super::mcp_refresh::McpRefreshInvalidationGuard;
use super::*;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::combine_selected_capability_roots;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::MAX_SELECTED_CAPABILITY_ROOTS;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_otel::auth_storage::AuthStorageOriginator;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;

impl Session {
    pub(crate) async fn runtime_mcp_config(&self, config: &Config) -> McpConfig {
        self.runtime_mcp_config_and_context(config).await.0
    }

    pub(crate) async fn runtime_mcp_config_and_context(
        &self,
        config: &Config,
    ) -> (McpConfig, McpRuntimeContext) {
        let host_fallback_cwd = self.state.lock().await.session_configuration.cwd().clone();
        let environments = self.services.turn_environments.snapshot().await;
        let mcp_config = self
            .services
            .mcp_manager
            .runtime_config_for_step(config, &environments.all_selections());
        let mcp_config = self
            .project_selected_environment_mcp_servers(&environments, mcp_config)
            .await;
        let runtime_context = self.mcp_runtime_context(&environments, &host_fallback_cwd);
        (mcp_config, runtime_context)
    }

    pub(crate) async fn current_mcp_runtime_context(&self) -> McpRuntimeContext {
        let host_fallback_cwd = self.state.lock().await.session_configuration.cwd().clone();
        let environments = self.services.turn_environments.snapshot().await;
        self.mcp_runtime_context(&environments, &host_fallback_cwd)
    }

    fn mcp_runtime_context(
        &self,
        environments: &TurnEnvironmentSnapshot,
        host_fallback_cwd: &std::path::Path,
    ) -> McpRuntimeContext {
        let local_process_cwd = environments
            .local_environment_cwd()
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| host_fallback_cwd.to_path_buf());
        McpRuntimeContext::new(
            self.services.turn_environments.environment_manager(),
            local_process_cwd,
        )
        .with_selected_environments(
            environments.all_selections().into(),
            environments.ready_environment_handles(),
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
            let environments = self.services.turn_environments.snapshot().await;
            let environment_selections = environments.all_selections();
            if !self.services.mcp_runtime.current_environments_match(
                &environment_selections,
                &environments.ready_environment_handles(),
            ) {
                self.mark_mcp_runtime_dirty();
            }
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
                .runtime_config_for_step(&desired.config, &environment_selections);
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

    #[tracing::instrument(
        name = "capability_roots.snapshot_for_step",
        skip_all,
        fields(root_count = ready_selected_capability_roots.len())
    )]
    pub(crate) async fn executor_capability_discovery_for_step(
        &self,
        config: &Config,
        ready_selected_capability_roots: &[SelectedCapabilityRoot],
    ) -> Option<Arc<ExecutorCapabilityDiscoverySnapshot>> {
        if !config
            .features
            .enabled(Feature::ExecutorCapabilityDiscovery)
        {
            return None;
        }
        let environment_manager = self.services.turn_environments.environment_manager();
        let cache = self
            .services
            .thread_extension_data
            .get_or_init(|| ExecutorCapabilityDiscoveryCache::new(environment_manager));
        Some(Arc::new(
            cache.snapshot(ready_selected_capability_roots).await,
        ))
    }

    pub(crate) async fn resolve_selected_capability_roots_for_step(
        &self,
        environments: &TurnEnvironmentSnapshot,
    ) -> Vec<ResolvedSelectedCapabilityRoot> {
        let captured_environments = environments.captured_environments();
        let thread_roots = environments.selected_capability_roots();
        let thread_root_count = thread_roots.len();
        let mut root_locations_by_id = HashMap::new();
        let mut selected_capability_roots = Vec::new();
        let mut ready_environment_root_count = 0;
        let combined_roots = combine_selected_capability_roots(
            &thread_roots,
            environments.turn_environments().map(|environment| {
                environment
                    .config_origin
                    .selected_capability_roots(&environment.environment, environment.config())
            }),
        );
        for (index, root) in combined_roots.into_iter().enumerate() {
            let CapabilityRootLocation::Environment { environment_id, .. } = &root.location;
            // Filter before readiness resolution, which can reconnect a historical executor.
            if !captured_environments.contains_key(environment_id) {
                tracing::warn!(
                    root_id = root.id,
                    environment_id,
                    "ignoring capability root without a captured turn environment"
                );
                continue;
            }
            if let Some(kept_location) = root_locations_by_id.get(&root.id) {
                if kept_location != &root.location {
                    tracing::warn!(
                        root_id = root.id,
                        ?kept_location,
                        ignored_location = ?root.location,
                        "ignoring selected capability root with conflicting location"
                    );
                }
                continue;
            }
            if index >= thread_root_count {
                if ready_environment_root_count == MAX_SELECTED_CAPABILITY_ROOTS {
                    tracing::warn!(
                        max_root_count = MAX_SELECTED_CAPABILITY_ROOTS,
                        "ignoring excess selected capability roots from ready environments"
                    );
                    break;
                }
                ready_environment_root_count += 1;
            }
            root_locations_by_id.insert(root.id.clone(), root.location.clone());
            selected_capability_roots.push(root);
        }
        self.services
            .turn_environments
            .environment_manager()
            .resolve_selected_capability_roots(&selected_capability_roots, &captured_environments)
            .await
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
        let environments = self.services.turn_environments.snapshot().await;
        let environment_selections = environments.all_selections();
        let desired = self.latest_mcp_desired_state(auth, environments).await;
        let mcp_config = self
            .services
            .mcp_manager
            .runtime_config_for_step(&desired.config, &environment_selections);
        self.publish_mcp_runtime(&desired, mcp_config).await;
    }

    pub(crate) fn ready_selected_capability_roots(
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
    ) -> Vec<SelectedCapabilityRoot> {
        selected_capability_roots
            .iter()
            .map(|root| root.selected_root().clone())
            .collect()
    }

    pub(crate) fn cancel_mcp_startup(&self) {
        self.services.mcp_runtime.cancel_startup();
    }
}

use super::mcp_refresh::McpRefreshInvalidationGuard;
use super::*;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::combine_selected_capability_roots;
use codex_exec_server::ExecutorCapabilityDiscoveryCache;
use codex_exec_server::ExecutorCapabilityDiscoverySnapshot;
use codex_exec_server::MAX_SELECTED_CAPABILITY_ROOTS;
use codex_exec_server::ResolvedSelectedCapabilityRoot;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_otel::auth_storage::AuthStorageOriginator;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_tools::REQUEST_PLUGIN_INSTALL_APPROVAL_KIND_VALUE;
use serde_json::Map;

// Wire keys of `codex_tools::RequestPluginInstallMeta`.
const PLUGIN_INSTALL_KIND_KEY: &str = "codex_approval_kind";
const PLUGIN_INSTALL_ACTION_KEY: &str = "suggest_type";
const PLUGIN_INSTALL_ACTION_INSTALL: &str = "install";
const PLUGIN_INSTALL_TOOL_ID_KEY: &str = "tool_id";
const PLUGIN_INSTALL_TOOL_NAME_KEY: &str = "tool_name";
const PLUGIN_INSTALL_TOOL_TYPE_KEY: &str = "tool_type";

pub(crate) struct McpServerElicitationOutcome {
    pub(crate) response: Option<ElicitationResponse>,
    pub(crate) sent: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct PluginInstallElicitationTelemetryMetadata {
    tool_type: String,
    tool_id: String,
    tool_name: String,
}

impl Session {
    pub(crate) async fn runtime_mcp_config(&self, config: &Config) -> McpConfig {
        self.runtime_mcp_config_and_context(config).await.0
    }

    pub(crate) async fn runtime_mcp_config_and_context(
        &self,
        config: &Config,
    ) -> (McpConfig, McpRuntimeContext) {
        let originator = self.originator().await;
        let (session_source, host_fallback_cwd, disabled_plugin_ids) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.session_source.clone(),
                state.session_configuration.cwd().clone(),
                state.active_disabled_plugin_ids.clone(),
            )
        };
        let environments = self.services.turn_environments.snapshot().await;
        let environment_selections = environments.all_selections();
        let selected_capability_roots = self
            .resolve_selected_capability_roots_for_step(&environments)
            .await;
        let ready_selected_capability_roots =
            Self::ready_selected_capability_roots(&selected_capability_roots);
        let executor_capability_discovery = self
            .executor_capability_discovery_for_step(config, &ready_selected_capability_roots)
            .await;
        let mcp_projection =
            self.services
                .mcp_manager
                .runtime_config_for_step(
                    config,
                    &self.services.mcp_thread_init,
                    &self.services.thread_extension_data,
                    McpThreadIdentity {
                        auth_changed: !self.services.mcp_runtime.current_auth_matches(
                            self.services.auth_manager.auth_cached().as_ref(),
                        ),
                        session_source: &session_source,
                        originator: &originator,
                        disabled_plugin_ids: &disabled_plugin_ids,
                        environments: McpEnvironmentScope::Selected(&environment_selections),
                    },
                    &ready_selected_capability_roots,
                    executor_capability_discovery.as_deref(),
                )
                .await;
        let mcp_config = self
            .project_selected_environment_mcp_servers(&environments, mcp_projection)
            .await
            .config;
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
            let selected_capability_roots = self
                .resolve_selected_capability_roots_for_step(&desired.environments)
                .await;
            let ready_selected_capability_roots =
                Self::ready_selected_capability_roots(&selected_capability_roots);
            let executor_capability_discovery = self
                .executor_capability_discovery_for_step(
                    &desired.config,
                    &ready_selected_capability_roots,
                )
                .await;
            let mcp_projection = self
                .services
                .mcp_manager
                .runtime_config_for_step(
                    &desired.config,
                    &self.services.mcp_thread_init,
                    &self.services.thread_extension_data,
                    McpThreadIdentity {
                        auth_changed: !self
                            .services
                            .mcp_runtime
                            .current_auth_matches(desired.auth.as_ref()),
                        session_source: &desired.session_source,
                        originator: &desired.originator,
                        disabled_plugin_ids: &desired.disabled_plugin_ids,
                        environments: McpEnvironmentScope::Selected(&environment_selections),
                    },
                    &ready_selected_capability_roots,
                    executor_capability_discovery.as_deref(),
                )
                .await;
            self.publish_mcp_runtime(&desired, mcp_projection, &ready_selected_capability_roots)
                .await;
            refresh_invalidation.published = true;
        }
    }

    /// Refreshes Apps tools on the published thread runtime and returns that client's snapshot.
    pub(crate) async fn refresh_codex_apps_tools(
        self: &Arc<Self>,
    ) -> anyhow::Result<codex_mcp::CodexAppsToolSnapshot> {
        // Reconcile unchanged config so failed or closed clients can be replaced.
        self.mark_mcp_runtime_dirty();
        self.refresh_mcp_if_dirty().await;
        let _refresh = self
            .mcp_refresh
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("MCP runtime refresh semaphore closed"))?;
        self.services.mcp_runtime.refresh_codex_apps_tools().await
    }

    /// Reconnects the runtime so refreshed Apps tools belong to their new exact client.
    pub(crate) async fn hard_refresh_latest_codex_apps_tools(
        self: &Arc<Self>,
    ) -> anyhow::Result<Vec<codex_mcp::ToolInfo>> {
        self.refresh_mcp_if_dirty().await;
        let _refresh = self
            .mcp_refresh
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("MCP runtime refresh semaphore closed"))?;
        let auth = self.services.auth_manager.auth().await;
        let environments = self.services.turn_environments.snapshot().await;
        let environment_selections = environments.all_selections();
        let desired = self.latest_mcp_desired_state(auth, environments).await;
        let selected_capability_roots = self
            .resolve_selected_capability_roots_for_step(&desired.environments)
            .await;
        let ready_selected_capability_roots =
            Self::ready_selected_capability_roots(&selected_capability_roots);
        let executor_capability_discovery = self
            .executor_capability_discovery_for_step(
                &desired.config,
                &ready_selected_capability_roots,
            )
            .await;
        let mcp_projection = self
            .services
            .mcp_manager
            .runtime_config_for_step(
                &desired.config,
                &self.services.mcp_thread_init,
                &self.services.thread_extension_data,
                McpThreadIdentity {
                    auth_changed: !self
                        .services
                        .mcp_runtime
                        .current_auth_matches(desired.auth.as_ref()),
                    session_source: &desired.session_source,
                    originator: &desired.originator,
                    disabled_plugin_ids: &desired.disabled_plugin_ids,
                    environments: McpEnvironmentScope::Selected(&environment_selections),
                },
                &ready_selected_capability_roots,
                executor_capability_discovery.as_deref(),
            )
            .await;
        let mcp_projection = self
            .project_selected_environment_mcp_servers(&desired.environments, mcp_projection)
            .await;
        let selected_plugins = mcp_projection.selected_plugins.clone();
        let input = self.build_mcp_runtime_input(
            &desired,
            mcp_projection,
            &ready_selected_capability_roots,
        );
        anyhow::ensure!(
            input.mcp_servers.contains_key(CODEX_APPS_MCP_SERVER_NAME),
            "unknown MCP server '{CODEX_APPS_MCP_SERVER_NAME}'"
        );
        let refreshed = AuthStorageOriginator::from_client_name(&desired.originator)
            .scope(self.services.mcp_runtime.replace_fresh(input))
            .await;
        self.services.thread_extension_data.insert(selected_plugins);
        refreshed
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
        selected_capability_roots: &[ResolvedSelectedCapabilityRoot],
        required_servers: &[String],
        required_plugins: &HashSet<String>,
    ) -> Arc<codex_mcp::McpBinding> {
        let ready_selected_capability_roots =
            Self::ready_selected_capability_roots(selected_capability_roots);
        if self
            .services
            .mcp_runtime
            .current_ready_selected_capability_roots()
            != ready_selected_capability_roots
        {
            self.mark_mcp_runtime_dirty();
        }

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
            .current_binding_with_requirements(&required_servers, required_plugins)
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
        let discovery = cache.snapshot(ready_selected_capability_roots).await;
        if cache.take_recovered_discovery() {
            // Root selection is unchanged, but recovered manifests can change MCP servers.
            self.mark_mcp_runtime_dirty();
        }
        Some(Arc::new(discovery))
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

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_mcp_server_elicitation(
        &self,
        turn_context: &TurnContext,
        server_name: String,
        request_id: RequestId,
        request: ElicitationRequest,
    ) -> McpServerElicitationOutcome {
        if self.services.mcp_runtime.elicitations_auto_deny() {
            return McpServerElicitationOutcome {
                response: Some(ElicitationResponse {
                    action: codex_rmcp_client::ElicitationAction::Accept,
                    content: Some(serde_json::json!({})),
                    meta: None,
                }),
                sent: false,
            };
        }

        let _elicitation = self.services.elicitations.register();
        let (tx_response, rx_response) = oneshot::channel();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_elicitation(
                        server_name.clone(),
                        request_id.clone(),
                        tx_response,
                    )
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!(
                "Overwriting existing pending elicitation for server_name: {server_name}, request_id: {request_id}"
            );
        }
        let id = match request_id {
            rmcp::model::NumberOrString::String(value) => {
                codex_protocol::mcp::RequestId::String(value.to_string())
            }
            rmcp::model::NumberOrString::Number(value) => {
                codex_protocol::mcp::RequestId::Integer(value)
            }
        };
        let event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
            turn_id: Some(turn_context.sub_id.clone()),
            server_name,
            id,
            request,
        });
        let plugin_install_telemetry = plugin_install_elicitation_telemetry_metadata(&event);
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        if let Some(plugin_install_telemetry) = plugin_install_telemetry {
            turn_context
                .session_telemetry
                .record_plugin_install_elicitation_sent(
                    plugin_install_telemetry.tool_type.as_str(),
                    plugin_install_telemetry.tool_id.as_str(),
                    plugin_install_telemetry.tool_name.as_str(),
                );
        }
        McpServerElicitationOutcome {
            response: rx_response.await.ok(),
            sent: true,
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and manager fallback must stay serialized"
    )]
    pub async fn resolve_elicitation(
        &self,
        server_name: String,
        id: RequestId,
        response: ElicitationResponse,
    ) -> anyhow::Result<()> {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_elicitation(&server_name, &id)
                }
                None => None,
            }
        };
        if let Some(tx_response) = entry {
            tx_response
                .send(response)
                .map_err(|e| anyhow::anyhow!("failed to send elicitation response: {e:?}"))?;
            return Ok(());
        }

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
        let ready_selected_capability_roots = self
            .services
            .mcp_runtime
            .current_ready_selected_capability_roots();
        let environments = self.services.turn_environments.snapshot().await;
        let environment_selections = environments.all_selections();
        let desired = self.latest_mcp_desired_state(auth, environments).await;
        let executor_capability_discovery = self
            .executor_capability_discovery_for_step(
                &desired.config,
                &ready_selected_capability_roots,
            )
            .await;
        let mcp_projection = self
            .services
            .mcp_manager
            .runtime_config_for_step(
                &desired.config,
                &self.services.mcp_thread_init,
                &self.services.thread_extension_data,
                McpThreadIdentity {
                    auth_changed: !self
                        .services
                        .mcp_runtime
                        .current_auth_matches(desired.auth.as_ref()),
                    session_source: &desired.session_source,
                    originator: &desired.originator,
                    disabled_plugin_ids: &desired.disabled_plugin_ids,
                    environments: McpEnvironmentScope::Selected(&environment_selections),
                },
                &ready_selected_capability_roots,
                executor_capability_discovery.as_deref(),
            )
            .await;
        self.publish_mcp_runtime(&desired, mcp_projection, &ready_selected_capability_roots)
            .await;
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

fn metadata_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

fn metadata_owned_string(meta: &Map<String, Value>, key: &str) -> Option<String> {
    metadata_str(meta, key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn plugin_install_elicitation_telemetry_metadata(
    event: &EventMsg,
) -> Option<PluginInstallElicitationTelemetryMetadata> {
    let EventMsg::ElicitationRequest(ElicitationRequestEvent { request, .. }) = event else {
        return None;
    };
    let ElicitationRequest::Form {
        meta: Some(Value::Object(meta)),
        ..
    } = request
    else {
        return None;
    };
    if metadata_str(meta, PLUGIN_INSTALL_KIND_KEY)
        != Some(REQUEST_PLUGIN_INSTALL_APPROVAL_KIND_VALUE)
        || metadata_str(meta, PLUGIN_INSTALL_ACTION_KEY) != Some(PLUGIN_INSTALL_ACTION_INSTALL)
    {
        return None;
    }

    Some(PluginInstallElicitationTelemetryMetadata {
        tool_type: metadata_owned_string(meta, PLUGIN_INSTALL_TOOL_TYPE_KEY)?,
        tool_id: metadata_owned_string(meta, PLUGIN_INSTALL_TOOL_ID_KEY)?,
        tool_name: metadata_owned_string(meta, PLUGIN_INSTALL_TOOL_NAME_KEY)?,
    })
}

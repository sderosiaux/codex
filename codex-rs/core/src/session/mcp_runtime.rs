//! Thread MCP runtime projection and publication.
//!
//! This module owns the small correctness boundary between immutable session
//! inputs and the mutable [`codex_mcp::McpRuntime`]. Background scheduling
//! belongs elsewhere.

use super::session::SessionConfiguration;
use super::*;
use codex_mcp::McpStartupPolicy;
use codex_mcp::PreparedMcpCall;
use codex_mcp::ToolInfo;
use codex_otel::auth_storage::AuthStorageOriginator;

pub(super) struct McpDesiredState {
    pub(super) config: Arc<Config>,
    pub(super) auth: Option<CodexAuth>,
    pub(super) submit_id: String,
    pub(super) originator: String,
    pub(super) session_source: SessionSource,
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
            .primary_cwd()
            .unwrap_or_else(|| session_configuration.cwd().clone())
            .to_path_buf();

        McpDesiredState {
            config: Arc::new(config),
            auth,
            submit_id: self.next_internal_sub_id(),
            originator: session_configuration.originator,
            session_source: session_configuration.session_source,
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
            .primary_cwd()
            .unwrap_or_else(|| session_configuration.cwd().clone())
            .to_path_buf();
        let desired = McpDesiredState {
            config: Arc::new(config),
            auth,
            submit_id: INITIAL_SUBMIT_ID.to_owned(),
            originator: session_configuration.originator.clone(),
            session_source: session_configuration.session_source.clone(),
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

    #[tracing::instrument(name = "mcp.runtime.refresh", skip_all)]
    pub(super) async fn publish_mcp_runtime(
        &self,
        desired: &McpDesiredState,
        mcp_config: McpConfig,
    ) {
        let input = self.build_mcp_runtime_input(desired, mcp_config);
        AuthStorageOriginator::from_client_name(&desired.originator)
            .scope(self.services.mcp_runtime.replace(input))
            .await;
    }

    pub(super) fn build_mcp_runtime_input(
        &self,
        desired: &McpDesiredState,
        config: McpConfig,
    ) -> McpRuntimeInput {
        let auth = desired.auth.clone();
        let mcp_servers = effective_mcp_servers(&config);
        let mcp_config = Arc::new(config);
        let runtime_context = self.new_mcp_runtime_context(desired.local_process_cwd.clone());
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

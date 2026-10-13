use std::sync::Arc;

use crate::agent::api::AgentControl;
use crate::agent::control::LocalAgentRuntime;
use crate::agents_md_manager::AgentsMdManager;
use crate::attestation::AttestationProvider;
use crate::client::ModelClient;
use crate::current_time::TimeProvider;
use crate::elicitation::ElicitationService;
use crate::environment_selection::ThreadEnvironments;
use crate::mcp::McpManager;
use crate::mcp_tool_exposure::McpHandlerCache;
use crate::tools::ExecutedToolCalls;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::unified_exec::UnifiedExecProcessManager;
use arc_swap::ArcSwap;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistry;
use codex_hooks::Hooks;
use codex_login::AuthManager;
use codex_mcp::McpRuntime;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_skills_extension::HostSkillsService;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use codex_utils_git_discovery::GitRootDiscovery;
use tokio::runtime::Handle;

pub(crate) struct SessionServices {
    /// The single owner of live MCP connections for this thread.
    pub(crate) mcp_runtime: Arc<McpRuntime>,
    /// Immutable MCP handlers scoped to this thread's current binding.
    pub(crate) mcp_handler_cache: McpHandlerCache,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    pub(crate) elicitations: ElicitationService,
    pub(crate) hooks: ArcSwap<Hooks>,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) git_root_discovery: Arc<GitRootDiscovery>,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) runtime_handle: Handle,
    pub(crate) skills_service: Arc<HostSkillsService>,
    pub(crate) agents_md_manager: Arc<AgentsMdManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<ExtensionRegistry<crate::config::Config>>,
    pub(crate) session_extension_data: ExtensionData,
    pub(crate) thread_extension_data: ExtensionData,
    /// MCP extensions fixed when this session is created.
    pub(crate) client_mcp_extensions: ClientMcpExtensions,
    /// Roots supplied when the thread started or loaded from saved history.
    /// Keep roots for unselected environments so selecting them again restores their capabilities.
    pub(crate) selected_capability_roots: Vec<SelectedCapabilityRoot>,
    pub(crate) agent_control: Arc<dyn AgentControl>,
    pub(crate) local_agent_runtime: LocalAgentRuntime,
    pub(crate) live_thread: Option<LiveThread>,
    pub(crate) thread_store: Arc<ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) time_provider: Arc<dyn TimeProvider>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) executed_tool_calls: ExecutedToolCalls,
    pub(crate) tool_search_handler_cache: ToolSearchHandlerCache,
    pub(crate) turn_environments: Arc<ThreadEnvironments>,
}

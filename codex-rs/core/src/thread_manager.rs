mod managed;
mod shared_instructions;
mod shutdown;

pub use shutdown::AgentTreeShutdown;
pub use shutdown::AgentTreeShutdownFailure;
pub use shutdown::AgentTreeShutdownFailureReason;
pub use shutdown::AgentTreeShutdownReport;

use crate::agent::AgentStatus;
use crate::agent::LocalAgentControl;
use crate::agent::api::AgentConfigUpdate;
use crate::agent::api::AgentControl;
use crate::agent::control::AgentControlInit;
use crate::agents_md_manager::SessionInstructions;
use crate::attestation::AttestationProvider;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::current_time::TimeProvider;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::environment_selection::default_thread_environment_selections;
use crate::mcp::McpManager;
use crate::session::GitEnrichmentPolicy;
use crate::session::INITIAL_SUBMIT_ID;
use crate::session::SessionIo;
use crate::session::SessionSpawnArgs;
use crate::session::resolve_multi_agent_version;
use crate::session::session::Session;
use crate::session::startup::SessionStartup;
use crate::session::startup::SessionStartupGuard;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use crate::thread_rollout_truncation::user_message_positions_in_rollout;
use crate::thread_startup_metadata::ThreadStartupMetadata;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::ThreadInstructionsProvider;
use codex_extension_api::UserInstructionsProvider;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_history::HistoryInitialization;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_login::AuthManager;
use codex_login::default_client::CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR;
use codex_login::default_client::originator;
use codex_model_provider::create_model_provider;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::W3cTraceContext;
use codex_skills_extension::HostSkillsService;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_utils_git_discovery::GitRootDiscovery;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::warn;

const THREAD_CREATED_CHANNEL_CAPACITY: usize = 1024;

pub(crate) type ThreadIdGenerator = Arc<dyn Fn() -> ThreadId + Send + Sync>;
type AgentControlFactory =
    Arc<dyn Fn(ThreadId) -> BoxFuture<'static, CodexResult<Arc<dyn AgentControl>>> + Send + Sync>;

pub(crate) fn default_thread_id_generator() -> ThreadIdGenerator {
    Arc::new(ThreadId::new)
}

/// Represents a newly created Codex thread (formerly called a conversation), including the first event
/// (which is [`EventMsg::SessionConfigured`]).
pub struct NewThread {
    pub thread_id: ThreadId,
    pub thread: Arc<CodexThread>,
    pub session_configured: SessionConfiguredEvent,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThreadShutdownReport {
    pub completed: Vec<ThreadId>,
    pub submit_failed: Vec<ThreadId>,
    pub timed_out: Vec<ThreadId>,
}

enum ShutdownOutcome {
    Complete,
    SubmitFailed,
    TimedOut,
}

/// [`ThreadManager`] is responsible for creating threads and maintaining
/// them in memory.
pub struct ThreadManager {
    state: Arc<ThreadManagerState>,
}

pub struct StartThreadOptions {
    pub config: Config,
    /// Host-owned provider for this root thread's additional instructions.
    ///
    /// Core composes these after the global [`UserInstructionsProvider`]
    /// snapshot and before repository instructions. `None` or blank provider
    /// output clears only the thread contribution. Core retains this provider
    /// and reads it when capturing model-request context. Hosts own fetching
    /// and caching, and must supply a provider again when cold-resuming a thread
    /// or forking an offline source. A live source can supply its applied snapshot
    /// to a fork, but its task-bound provider is never inherited by the new root.
    /// Registration lasts for the live session. Warm resume ignores this option
    /// and reuses the existing provider without loading it; normal model-request
    /// boundaries load its latest instructions.
    pub thread_instructions_provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    pub allow_provider_model_fallback: bool,
    pub initial_history: InitialHistory,
    pub history_mode: Option<ThreadHistoryMode>,
    pub session_source: Option<SessionSource>,
    pub thread_source: Option<ThreadSource>,
    pub dynamic_tools: Vec<codex_protocol::dynamic_tools::DynamicToolSpec>,
    pub metrics_service_name: Option<String>,
    pub parent_trace: Option<W3cTraceContext>,
    pub environments: Option<Vec<TurnEnvironmentSelection>>,
    /// Existing environment bindings captured by an internal caller.
    pub inherited_environments: Option<TurnEnvironmentSnapshot>,
    /// Explicit global instructions carried by an internal caller instead of loading them again.
    pub user_instructions: Option<LoadedUserInstructions>,
    pub thread_extension_init: ExtensionDataInit,
    /// Host-supplied data captured by each new turn. This is not persisted.
    pub turn_extension_init: ExtensionDataInit,
    pub client_mcp_extensions: ClientMcpExtensions,
    /// Thread ID reserved before startup so the caller can associate host-owned state with it.
    pub reserved_thread_id: Option<ThreadId>,
}

impl StartThreadOptions {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            thread_instructions_provider: None,
            allow_provider_model_fallback: false,
            initial_history: InitialHistory::New,
            history_mode: None,
            session_source: None,
            thread_source: None,
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            parent_trace: None,
            environments: None,
            inherited_environments: None,
            user_instructions: None,
            thread_extension_init: ExtensionDataInit::default(),
            turn_extension_init: ExtensionDataInit::default(),
            client_mcp_extensions: ClientMcpExtensions::default(),
            reserved_thread_id: None,
        }
    }
}

struct ThreadSpawnRequest {
    startup: Option<Arc<crate::session::startup::SessionStartup>>,
    options: StartThreadOptions,
    auth_manager: Arc<AuthManager>,
    agent_control: AgentControlInit,
    parent_thread_id: Option<ThreadId>,
    parent_originator: Option<String>,
    forked_from_thread_id: Option<ThreadId>,
    inherited_environments: Option<TurnEnvironmentSnapshot>,
    inherited_instructions: Option<SessionInstructions>,
    user_shell_override: Option<crate::shell::Shell>,
}

impl ThreadSpawnRequest {
    fn new(
        options: StartThreadOptions,
        auth_manager: Arc<AuthManager>,
        agent_control: impl Into<AgentControlInit>,
    ) -> Self {
        Self {
            startup: None,
            options,
            auth_manager,
            agent_control: agent_control.into(),
            parent_thread_id: None,
            parent_originator: None,
            forked_from_thread_id: None,
            inherited_environments: None,
            inherited_instructions: None,
            user_shell_override: None,
        }
    }
}

fn originator_from_service_name(service_name: Option<&str>) -> Option<String> {
    let service_name = service_name?.trim();
    for originator in [
        "codex_work_desktop",
        "codex_work_web",
        "codex_work_mobile",
        "codex_work_cca",
        "chatgpt_cca",
    ] {
        if service_name.eq_ignore_ascii_case(originator) {
            return Some(originator.to_string());
        }
    }
    None
}

fn effective_originator_value(
    metrics_service_name: Option<&str>,
    env_originator: Option<String>,
    persisted_originator: Option<String>,
    inherited_originator: Option<String>,
    default_originator: String,
) -> String {
    originator_from_service_name(metrics_service_name)
        .or(persisted_originator)
        .or(inherited_originator)
        .or(env_originator)
        .unwrap_or(default_originator)
}

pub(crate) struct ResumeThreadWithHistoryOptions {
    pub(crate) config: Config,
    pub(crate) initial_history: InitialHistory,
    pub(crate) agent_control: LocalAgentControl,
    pub(crate) session_source: SessionSource,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) environment_selections: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) inherited_environments: Option<TurnEnvironmentSnapshot>,
    pub(crate) inherited_instructions: Option<SessionInstructions>,
    pub(crate) client_mcp_extensions: Option<ClientMcpExtensions>,
}

/// Shared, `Arc`-owned state for [`ThreadManager`]. This `Arc` is required to have a single
/// `Arc` reference that can be downgraded to by `LocalAgentControl` while preventing every single
/// function to require an `Arc<&Self>`.
pub(crate) struct ThreadManagerState {
    // Eviction updates this registry and residency together, locking the registry first.
    pub(crate) threads: Arc<RwLock<HashMap<ThreadId, Arc<CodexThread>>>>,
    shared_thread_instructions: shared_instructions::SharedThreadInstructionsProviders,
    thread_created_tx: broadcast::Sender<ThreadId>,
    thread_id_generator: ThreadIdGenerator,
    agent_control_factory: Option<AgentControlFactory>,
    auth_manager: Arc<AuthManager>,
    models_manager: SharedModelsManager,
    git_root_discovery: Arc<GitRootDiscovery>,
    environment_manager: Arc<EnvironmentManager>,
    starting_mcp_runtimes: std::sync::Mutex<Vec<std::sync::Weak<AtomicBool>>>,
    skills_service: Arc<HostSkillsService>,
    mcp_manager: Arc<McpManager>,
    extensions: Arc<ExtensionRegistry<Config>>,
    user_instructions_provider: Arc<dyn UserInstructionsProvider>,
    thread_store: Arc<ThreadStore>,
    attestation_provider: Option<Arc<dyn AttestationProvider>>,
    external_time_provider: Option<Arc<dyn TimeProvider>>,
    session_source: SessionSource,
    installation_id: String,
}

pub fn build_models_manager(
    config: &Config,
    auth_manager: Arc<AuthManager>,
) -> SharedModelsManager {
    let provider = create_model_provider(config.model_provider.clone(), Some(auth_manager));
    let manager = provider.models_manager(
        config.codex_home.to_path_buf(),
        config.model_catalog.clone(),
    );
    manager.set_api_key_model_discovery_enabled(
        config.features.enabled(Feature::ApiKeyModelDiscovery),
    );
    manager
}

impl ThreadManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Config,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        session_source: SessionSource,
        environment_manager: Arc<EnvironmentManager>,
        extensions: Arc<ExtensionRegistry<Config>>,
        user_instructions_provider: Arc<dyn UserInstructionsProvider>,
        installation_id: String,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
    ) -> Self {
        let restriction_product = session_source.restriction_product();
        let (thread_created_tx, _) = broadcast::channel(THREAD_CREATED_CHANNEL_CAPACITY);
        let skills_service = Arc::new(HostSkillsService::new_with_restriction_product(
            config.codex_home.clone(),
            config.bundled_skills_enabled(),
            restriction_product,
        ));
        let mcp_manager = Arc::new(McpManager::new());
        Self {
            state: Arc::new(ThreadManagerState {
                threads: Arc::new(RwLock::new(HashMap::new())),
                shared_thread_instructions: Default::default(),
                thread_created_tx,
                thread_id_generator: default_thread_id_generator(),
                agent_control_factory: None,
                models_manager,
                git_root_discovery: Arc::default(),
                environment_manager,
                starting_mcp_runtimes: std::sync::Mutex::new(Vec::new()),
                skills_service,
                mcp_manager,
                extensions,
                user_instructions_provider,
                thread_store: Arc::new(ThreadStore::default()),
                attestation_provider,
                external_time_provider,
                auth_manager,
                session_source,
                installation_id,
            }),
        }
    }

    /// Generate every new thread identifier with the caller-provided factory.
    pub fn with_thread_id_generator(
        mut self,
        generator: impl Fn() -> ThreadId + Send + Sync + 'static,
    ) -> Self {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("thread ID generator must be set before thread manager is shared");
        };
        state.thread_id_generator = Arc::new(generator);
        self
    }

    /// Selects the host-owned agent controller for every new or cold-resumed thread.
    ///
    /// The factory receives the thread's final ID and must return the controller for
    /// its agent tree. New roots use their thread ID as the controller identity;
    /// resumed threads must match their persisted session identity. Factory errors
    /// fail startup. Internal sessions inherit their parent's controller, and warm
    /// resume retains the live controller without calling the factory.
    ///
    /// Install this before starting threads. Without a factory, the manager uses
    /// the local backend. Host backends support MultiAgentV2; V1 requires local control.
    pub fn with_agent_control_factory<F, Fut>(mut self, factory: F) -> Self
    where
        F: Fn(ThreadId) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CodexResult<Arc<dyn AgentControl>>> + Send + 'static,
    {
        let Some(state) = Arc::get_mut(&mut self.state) else {
            unreachable!("agent controller factory must be set before thread manager is shared");
        };
        state.agent_control_factory = Some(Arc::new(move |thread_id| Box::pin(factory(thread_id))));
        self
    }

    pub fn session_source(&self) -> SessionSource {
        self.state.session_source.clone()
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.state.auth_manager.clone()
    }

    pub fn skills_service(&self) -> Arc<HostSkillsService> {
        self.state.skills_service.clone()
    }

    pub fn mcp_manager(&self) -> Arc<McpManager> {
        self.state.mcp_manager.clone()
    }

    pub fn environment_manager(&self) -> Arc<EnvironmentManager> {
        self.state.environment_manager.clone()
    }

    /// Refreshes every loaded thread and marks threads that are still being created.
    pub async fn invalidate_mcp_runtimes(&self) {
        self.invalidate_starting_mcp_runtimes();
        let threads = self
            .state
            .threads
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for thread in threads {
            thread.session.request_mcp_runtime_refresh();
        }
    }

    /// Rebuilds loaded hook runtimes without reloading their session configurations.
    pub async fn refresh_hook_runtimes(&self) {
        let threads = self
            .state
            .threads
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for thread in threads {
            let config = thread.session.get_config().await;
            thread.session.refresh_hooks(config).await;
        }
    }

    fn invalidate_starting_mcp_runtimes(&self) {
        let mut starting = self
            .state
            .starting_mcp_runtimes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        starting.retain(|runtime| {
            let Some(runtime) = runtime.upgrade() else {
                return false;
            };
            runtime.store(true, Ordering::Release);
            true
        });
    }

    pub fn get_models_manager(&self) -> SharedModelsManager {
        self.state.models_manager.clone()
    }

    pub async fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> Vec<ModelPreset> {
        self.state
            .models_manager
            .list_models(refresh_strategy, http_client_factory)
            .await
    }

    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.state.models_manager.list_collaboration_modes()
    }

    pub async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.state.list_thread_ids().await
    }

    pub fn subscribe_thread_created(&self) -> broadcast::Receiver<ThreadId> {
        self.state.thread_created_tx.subscribe()
    }

    pub async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        self.state.get_thread(thread_id).await
    }

    /// List `thread_id` plus all known descendants in its spawn subtree.
    pub async fn list_agent_subtree_thread_ids(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut subtree_thread_ids = Vec::new();
        let mut seen_thread_ids = HashSet::new();
        subtree_thread_ids.push(thread_id);
        seen_thread_ids.insert(thread_id);

        for descendant_id in self
            .agent_control()
            .runtime
            .list_live_agent_subtree_thread_ids(thread_id)
            .await?
        {
            if seen_thread_ids.insert(descendant_id) {
                subtree_thread_ids.push(descendant_id);
            }
        }

        Ok(subtree_thread_ids)
    }

    pub async fn start_thread(&self, options: StartThreadOptions) -> CodexResult<NewThread> {
        Box::pin(self.start_thread_inner(
            options, /*forked_from_thread_id*/ None, /*startup*/ None,
        ))
        .await
    }

    /// Allocates a thread ID before startup so a caller can associate host-owned state with it.
    pub fn reserve_thread_id(&self) -> ThreadId {
        self.state.thread_id_generator.as_ref()()
    }

    async fn start_thread_inner(
        &self,
        mut options: StartThreadOptions,
        forked_from_thread_id: Option<ThreadId>,
        startup: Option<Arc<crate::session::startup::SessionStartup>>,
    ) -> CodexResult<NewThread> {
        let (resumed_session_source, resumed_thread_source) = options
            .initial_history
            .get_resumed_session_sources()
            .unwrap_or_else(|| (self.state.session_source.clone(), None));
        options.session_source = Some(
            options
                .session_source
                .take()
                .unwrap_or(resumed_session_source),
        );
        options.thread_source = options.thread_source.take().or(resumed_thread_source);
        let agent_control = self.agent_control_for_config(&options.config);
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.state.auth_manager), agent_control);
        request.forked_from_thread_id = forked_from_thread_id;
        request.startup = startup;
        Box::pin(self.state.spawn_thread(request)).await
    }

    /// Removes the thread from the manager's internal map, though the thread is stored
    /// as `Arc<CodexThread>`, it is possible that other references to it exist elsewhere.
    /// Returns the thread if the thread was found and removed.
    pub async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        self.state.threads.write().await.remove(thread_id)
    }

    /// Removes a thread only if `thread_id` still maps to `expected`.
    ///
    /// Delayed cleanup uses this to avoid removing a replacement runtime registered under the
    /// same thread ID.
    pub async fn remove_thread_if_matches(
        &self,
        thread_id: &ThreadId,
        expected: &Arc<CodexThread>,
    ) -> Option<Arc<CodexThread>> {
        let mut threads = self.state.threads.write().await;
        if threads
            .get(thread_id)
            .is_some_and(|thread| Arc::ptr_eq(thread, expected))
        {
            threads.remove(thread_id)
        } else {
            None
        }
    }

    /// Tries to shut down all tracked threads concurrently within the provided timeout.
    /// Threads that complete shutdown are removed from the manager; incomplete shutdowns
    /// remain tracked so callers can retry or inspect them later.
    pub async fn shutdown_all_threads_bounded(&self, timeout: Duration) -> ThreadShutdownReport {
        let threads = {
            let threads = self.state.threads.read().await;
            threads
                .iter()
                .map(|(thread_id, thread)| (*thread_id, Arc::clone(thread)))
                .collect::<Vec<_>>()
        };

        let mut shutdowns = threads
            .into_iter()
            .map(|(thread_id, thread)| async move {
                let outcome = match tokio::time::timeout(timeout, thread.shutdown_and_wait()).await
                {
                    Ok(Ok(())) => ShutdownOutcome::Complete,
                    Ok(Err(_)) => ShutdownOutcome::SubmitFailed,
                    Err(_) => ShutdownOutcome::TimedOut,
                };
                (thread_id, outcome)
            })
            .collect::<FuturesUnordered<_>>();
        let mut report = ThreadShutdownReport::default();

        while let Some((thread_id, outcome)) = shutdowns.next().await {
            match outcome {
                ShutdownOutcome::Complete => report.completed.push(thread_id),
                ShutdownOutcome::SubmitFailed => report.submit_failed.push(thread_id),
                ShutdownOutcome::TimedOut => report.timed_out.push(thread_id),
            }
        }

        let mut tracked_threads = self.state.threads.write().await;
        for thread_id in &report.completed {
            tracked_threads.remove(thread_id);
        }

        report
            .completed
            .sort_by_key(std::string::ToString::to_string);
        report
            .submit_failed
            .sort_by_key(std::string::ToString::to_string);
        report
            .timed_out
            .sort_by_key(std::string::ToString::to_string);
        report
    }

    pub(crate) fn agent_control(&self) -> LocalAgentControl {
        LocalAgentControl::new(
            Arc::downgrade(&self.state),
            self.state.thread_id_generator.clone(),
            /*rollout_budget*/ None,
        )
    }

    fn agent_control_for_config(&self, config: &Config) -> LocalAgentControl {
        LocalAgentControl::new(
            Arc::downgrade(&self.state),
            self.state.thread_id_generator.clone(),
            config.rollout_budget.clone(),
        )
    }
}

impl ThreadManagerState {
    pub(crate) fn shared_thread_instructions_provider(
        &self,
        root_thread_id: ThreadId,
        provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> Option<Arc<dyn ThreadInstructionsProvider>> {
        self.shared_thread_instructions
            .for_root(root_thread_id, provider)
    }

    pub(crate) async fn list_thread_ids(&self) -> Vec<ThreadId> {
        self.threads.read().await.keys().copied().collect()
    }

    /// List parent-child edges for currently loaded thread-spawn agents.
    pub(crate) async fn list_live_thread_spawn_edges(&self) -> Vec<(ThreadId, ThreadId)> {
        self.threads
            .read()
            .await
            .iter()
            .filter_map(|(thread_id, thread)| match &thread.session_source {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                }) => Some((*parent_thread_id, *thread_id)),
                _ => None,
            })
            .collect()
    }

    /// Fetch a thread by ID or return ThreadNotFound.
    pub(crate) async fn get_thread(&self, thread_id: ThreadId) -> CodexResult<Arc<CodexThread>> {
        let threads = self.threads.read().await;
        match threads.get(&thread_id) {
            Some(thread) => Ok(thread.clone()),
            None => Err(CodexErr::ThreadNotFound(thread_id)),
        }
    }

    pub(crate) async fn read_stored_thread(
        &self,
        thread_id: ThreadId,
        include_history: bool,
    ) -> CodexResult<StoredThread> {
        self.thread_store
            .read_thread(thread_id, include_history)
            .await
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                err => CodexErr::Fatal(format!("failed to read stored thread {thread_id}: {err}")),
            })
    }

    pub(crate) async fn load_thread_history(
        &self,
        thread_id: ThreadId,
    ) -> CodexResult<Vec<RolloutItem>> {
        self.thread_store
            .load_history(thread_id)
            .await
            .map(|history| history.items)
            .map_err(|err| match err {
                ThreadStoreError::ThreadNotFound { thread_id } => {
                    CodexErr::ThreadNotFound(thread_id)
                }
                err => CodexErr::Fatal(format!(
                    "failed to load history for thread {thread_id}: {err}"
                )),
            })
    }

    /// Send an operation to a thread by ID.
    pub(crate) async fn send_op(
        &self,
        thread_id: ThreadId,
        op: Op,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
    ) -> CodexResult<String> {
        let thread = self.get_thread(thread_id).await?;
        let residency_guard = if matches!(op, Op::InterAgentCommunication { .. }) {
            thread
                .session
                .services
                .local_agent_runtime
                .pin_v2_residency(self, &thread)
                .await?
        } else {
            None
        };
        thread
            .io
            .submit_with_trace(
                op,
                /*trace*/ None,
                parent_turn_id,
                root_turn_id,
                residency_guard,
            )
            .await
    }

    /// Remove a thread from the manager by ID, returning it when present.
    pub(crate) async fn remove_thread(&self, thread_id: &ThreadId) -> Option<Arc<CodexThread>> {
        self.threads.write().await.remove(thread_id)
    }

    pub(crate) async fn effective_multi_agent_version_for_spawn(
        &self,
        initial_history: &InitialHistory,
        session_source: Option<&SessionSource>,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        config: &Config,
    ) -> MultiAgentVersion {
        if let Some(multi_agent_version) = config.multi_agent_version_override() {
            return multi_agent_version;
        }
        self.initial_multi_agent_version_for_spawn(
            initial_history,
            session_source,
            parent_thread_id,
            forked_from_thread_id,
        )
        .await
        .unwrap_or_else(|| config.multi_agent_version_from_features())
    }

    async fn initial_multi_agent_version_for_spawn(
        &self,
        initial_history: &InitialHistory,
        session_source: Option<&SessionSource>,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
    ) -> Option<MultiAgentVersion> {
        let inherited_thread_id = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            })) => Some(*parent_thread_id),
            _ => match initial_history {
                InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
                InitialHistory::Forked(_) => forked_from_thread_id.or(parent_thread_id),
                InitialHistory::New | InitialHistory::Cleared => parent_thread_id,
            },
        };
        let inherited_multi_agent_version = match inherited_thread_id {
            Some(thread_id) => self
                .get_thread(thread_id)
                .await
                .ok()
                .and_then(|thread| thread.multi_agent_version()),
            None => None,
        };
        resolve_multi_agent_version(initial_history, inherited_multi_agent_version)
    }

    /// Selects instruction sources for a newly spawned runtime without loading providers.
    ///
    /// Fresh roots, cold resumes, and root forks retain the global and any supplied
    /// thread provider. The session's AgentsMdManager loads them at startup and
    /// subsequent context captures. Subagents inherit applied parent snapshots and
    /// any thread provider that explicitly opts into sharing with descendants.
    /// A root fork without a provider preserves the live source's thread snapshot,
    /// never its task-bound provider. Hosts must supply a provider for offline forks.
    /// Warm resumes retain the existing session and do not call this function.
    async fn instructions_for_spawn(
        &self,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        thread_provider: Option<Arc<dyn ThreadInstructionsProvider>>,
    ) -> SessionInstructions {
        let inherited_thread_id = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) => Some(*parent_thread_id),
            _ => parent_thread_id.or(forked_from_thread_id),
        };
        let inherited = match inherited_thread_id {
            Some(thread_id) => match self.get_thread(thread_id).await {
                Ok(thread) => thread.session.inherited_instructions().await,
                Err(_) => SessionInstructions::default(),
            },
            None => SessionInstructions::default(),
        };
        if session_source.is_non_root_agent() {
            inherited
        } else {
            SessionInstructions {
                user_provider: Some(Arc::clone(&self.user_instructions_provider)),
                thread_provider,
                thread: inherited.thread,
                ..Default::default()
            }
        }
    }

    async fn inherited_originator_for_parent_thread(
        &self,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
    ) -> Option<String> {
        let inherited_thread_id = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) => Some(*parent_thread_id),
            _ => parent_thread_id.or(forked_from_thread_id),
        };
        let thread = self.get_thread(inherited_thread_id?).await.ok()?;
        let originator = thread.config_snapshot().await.originator;
        (!originator.is_empty()).then_some(originator)
    }

    async fn effective_originator(
        &self,
        initial_history: &InitialHistory,
        metrics_service_name: Option<&str>,
        session_source: &SessionSource,
        parent_thread_id: Option<ThreadId>,
        forked_from_thread_id: Option<ThreadId>,
        parent_originator: Option<String>,
    ) -> String {
        let persisted_originator = initial_history.get_session_originator();
        let inherited_originator = match initial_history {
            InitialHistory::New | InitialHistory::Cleared => {
                self.inherited_originator_for_parent_thread(
                    session_source,
                    parent_thread_id,
                    forked_from_thread_id,
                )
                .await
            }
            InitialHistory::Forked(_) if persisted_originator.is_none() => {
                self.inherited_originator_for_parent_thread(
                    session_source,
                    parent_thread_id,
                    forked_from_thread_id,
                )
                .await
            }
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => None,
        };

        let env_originator = std::env::var(CODEX_INTERNAL_ORIGINATOR_OVERRIDE_ENV_VAR)
            .is_ok()
            .then(|| originator().value);
        effective_originator_value(
            metrics_service_name,
            env_originator,
            persisted_originator,
            inherited_originator.or(parent_originator),
            originator().value,
        )
    }

    /// Start a newly spawned agent. Both history modes use the same options captured
    /// from the spawning turn; only history ownership and persistence differ.
    pub(crate) async fn spawn_child_thread(
        &self,
        mut options: StartThreadOptions,
        agent_control: LocalAgentControl,
        parent_thread_id: Option<ThreadId>,
    ) -> CodexResult<NewThread> {
        options.client_mcp_extensions =
            self.client_mcp_extensions_for_child(parent_thread_id).await;
        let forked = matches!(options.initial_history, InitialHistory::Forked(_));
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.auth_manager), agent_control);
        request.parent_thread_id = parent_thread_id;
        if forked {
            request.forked_from_thread_id = parent_thread_id;
        }
        Box::pin(self.spawn_thread(request)).await
    }

    pub(crate) async fn resume_thread_with_history_with_source(
        &self,
        options: ResumeThreadWithHistoryOptions,
    ) -> CodexResult<NewThread> {
        let ResumeThreadWithHistoryOptions {
            config,
            initial_history,
            agent_control,
            session_source,
            parent_thread_id,
            environment_selections,
            inherited_environments,
            inherited_instructions,
            client_mcp_extensions,
        } = options;
        let client_mcp_extensions = match client_mcp_extensions {
            Some(client_mcp_extensions) => client_mcp_extensions,
            None => self.client_mcp_extensions_for_child(parent_thread_id).await,
        };
        let thread_source = initial_history.get_resumed_thread_source();
        let options = StartThreadOptions {
            initial_history,
            session_source: Some(session_source),
            thread_source,
            environments: environment_selections,
            client_mcp_extensions,
            ..StartThreadOptions::new(config)
        };
        let mut request =
            ThreadSpawnRequest::new(options, Arc::clone(&self.auth_manager), agent_control);
        request.parent_thread_id = parent_thread_id;
        request.inherited_environments = inherited_environments;
        request.inherited_instructions = inherited_instructions;
        Box::pin(self.spawn_thread(request)).await
    }

    async fn client_mcp_extensions_for_child(
        &self,
        parent_thread_id: Option<ThreadId>,
    ) -> ClientMcpExtensions {
        let Some(parent_thread_id) = parent_thread_id else {
            return ClientMcpExtensions::default();
        };
        self.get_thread(parent_thread_id)
            .await
            .map(|parent| parent.session.services.client_mcp_extensions.clone())
            .unwrap_or_default()
    }

    /// Spawn a new thread with optional history and register it with the manager.
    async fn spawn_thread(&self, mut request: ThreadSpawnRequest) -> CodexResult<NewThread> {
        let runtime = request.agent_control.runtime().clone();
        let membership = runtime.admit_start()?;
        let owns_startup = request.startup.is_none();
        let startup_state = Arc::clone(
            request
                .startup
                .get_or_insert_with(|| Arc::new(SessionStartup::default())),
        );
        startup_state.hold_membership(membership);
        let startup_guard =
            owns_startup.then(|| SessionStartupGuard::new(Arc::clone(&startup_state)));
        let ThreadSpawnRequest {
            startup,
            options,
            auth_manager,
            agent_control,
            parent_thread_id,
            parent_originator,
            forked_from_thread_id,
            inherited_environments,
            inherited_instructions,
            user_shell_override,
        } = request;
        let history_initialization = if matches!(
            &options.initial_history,
            InitialHistory::New | InitialHistory::Forked(_)
        ) && let Some(source_thread_id) = forked_from_thread_id
        {
            if let Ok(source) = self.get_thread(source_thread_id).await
                && source.agent_status().await != AgentStatus::Shutdown
            {
                HistoryInitialization::WarmFork
            } else {
                HistoryInitialization::ColdFork
            }
        } else {
            HistoryInitialization::from_history(&options.initial_history)
        };
        let StartThreadOptions {
            mut config,
            thread_instructions_provider,
            allow_provider_model_fallback,
            initial_history,
            history_mode,
            session_source,
            thread_source,
            dynamic_tools,
            metrics_service_name,
            parent_trace,
            environments: environment_selections,
            inherited_environments: captured_environments,
            user_instructions: supplied_user_instructions,
            mut thread_extension_init,
            turn_extension_init,
            client_mcp_extensions,
            mut reserved_thread_id,
        } = options;
        let inherited_environments = captured_environments.or(inherited_environments);
        let session_source = session_source.unwrap_or_else(|| self.session_source.clone());
        let isolation = thread_extension_init
            .get::<codex_extension_api::SessionIsolation>()
            .map(|policy| *policy)
            .unwrap_or(codex_extension_api::SessionIsolation::Inherit);
        thread_extension_init.insert(isolation);
        let environment_selections = environment_selections
            .or_else(|| {
                let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. }) = &session_source
                else {
                    return None;
                };
                inherited_environments
                    .as_ref()
                    .map(TurnEnvironmentSnapshot::selections)
            })
            .unwrap_or_else(|| {
                default_thread_environment_selections(&config.cwd, &config.workspace_roots)
            });
        let is_resumed_thread = matches!(&initial_history, InitialHistory::Resumed(_));
        if reserved_thread_id.is_some() && matches!(&initial_history, InitialHistory::Resumed(_)) {
            return Err(CodexErr::InvalidRequest(
                "reserved thread ID cannot be used when resuming a thread".to_string(),
            ));
        }
        if let InitialHistory::Resumed(resumed) = &initial_history {
            let mut threads = self.threads.write().await;
            if let Some(thread) = threads.get(&resumed.conversation_id).cloned() {
                if thread.is_running() {
                    let session_configured =
                        thread.startup_metadata().to_session_configured_event();
                    startup_state.release_membership();
                    if let Some(startup_guard) = startup_guard {
                        startup_guard.disarm();
                    }
                    return Ok(NewThread {
                        thread_id: resumed.conversation_id,
                        session_configured,
                        thread,
                    });
                }
                threads.remove(&resumed.conversation_id);
            }
        }
        let agent_control = match (&self.agent_control_factory, agent_control) {
            (Some(factory), AgentControlInit::Local(local)) => {
                let thread_id = match &initial_history {
                    InitialHistory::Resumed(resumed) => resumed.conversation_id,
                    InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                        *reserved_thread_id
                            .get_or_insert_with(|| local.runtime.generate_thread_id())
                    }
                };
                AgentControlInit::Provided {
                    control: factory(thread_id).await?,
                    runtime: local.runtime,
                }
            }
            (_, control) => control,
        };
        let parent_thread_id = parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id())
            .or(match &session_source {
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                }) => Some(*parent_thread_id),
                _ => None,
            });
        // Host controllers can already be shared with live threads. Publish root settings
        // only after registration succeeds, so a failed start cannot update their tree.
        let initial_host_config = (self.agent_control_factory.is_some()
            && parent_thread_id.is_none())
        .then(|| AgentConfigUpdate::ServiceTier(config.service_tier.clone()));
        let (instructions, extensions, mcp_manager, multi_agent_version) =
            if isolation == codex_extension_api::SessionIsolation::Isolated {
                let extensions = thread_extension_init
                    .get::<codex_extension_api::IsolatedSessionExtensions<Config>>()
                    .map(|extensions| Arc::clone(&extensions.0))
                    .unwrap_or_else(empty_extension_registry);
                let mcp_manager = McpManager::new();
                (
                    inherited_instructions.unwrap_or_default(),
                    extensions,
                    Arc::new(mcp_manager),
                    Some(MultiAgentVersion::Disabled),
                )
            } else {
                (
                    match inherited_instructions {
                        Some(instructions) => instructions,
                        None => {
                            self.instructions_for_spawn(
                                &session_source,
                                parent_thread_id,
                                forked_from_thread_id,
                                thread_instructions_provider,
                            )
                            .await
                        }
                    },
                    Arc::clone(&self.extensions),
                    Arc::clone(&self.mcp_manager),
                    self.initial_multi_agent_version_for_spawn(
                        &initial_history,
                        Some(&session_source),
                        parent_thread_id,
                        forked_from_thread_id,
                    )
                    .await,
                )
            };
        let mut instructions = instructions;
        if let Some(supplied) = supplied_user_instructions {
            instructions.user = supplied.instructions;
            instructions.user_provider = None;
            config.startup_warnings.extend(supplied.warnings);
        }
        let tracked_session_source = session_source.clone();
        let originator = self
            .effective_originator(
                &initial_history,
                metrics_service_name.as_deref(),
                &session_source,
                parent_thread_id,
                forked_from_thread_id,
                parent_originator,
            )
            .await;
        let source_changed_during_startup = Arc::new(AtomicBool::new(false));
        {
            let mut starting = self
                .starting_mcp_runtimes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            starting.retain(|runtime| runtime.strong_count() != 0);
            starting.push(Arc::downgrade(&source_changed_during_startup));
        }
        let spawn_result = Session::spawn(SessionSpawnArgs {
            startup,
            config,
            allow_provider_model_fallback,
            instructions,
            installation_id: self.installation_id.clone(),
            auth_manager,
            models_manager: Arc::clone(&self.models_manager),
            git_root_discovery: Arc::clone(&self.git_root_discovery),
            environment_manager: Arc::clone(&self.environment_manager),
            skills_service: Arc::clone(&self.skills_service),
            mcp_manager,
            extensions,
            conversation_history: initial_history,
            history_initialization,
            requested_history_mode: history_mode,
            session_source,
            forked_from_thread_id,
            parent_thread_id,
            thread_source: thread_source.clone(),
            originator,
            agent_control,
            dynamic_tools,
            metrics_service_name,
            inherited_environments,
            user_shell_override,
            parent_trace,
            environment_selections,
            thread_extension_init,
            turn_extension_init,
            client_mcp_extensions,
            reserved_thread_id,
            thread_store: Arc::clone(&self.thread_store),
            attestation_provider: self.attestation_provider.clone(),
            external_time_provider: self.external_time_provider.clone(),
            inherited_multi_agent_version: multi_agent_version,
            git_enrichment_policy: GitEnrichmentPolicy::Fresh,
        })
        .await;
        let (session, io) = match spawn_result {
            Ok(spawned) => spawned,
            Err(error) => {
                if let Some(startup_guard) = startup_guard {
                    startup_guard.cleanup().await;
                }
                return Err(error);
            }
        };
        let finalize_result = self
            .finalize_thread_spawn(session, io, tracked_session_source, initial_host_config)
            .await;
        let new_thread = match finalize_result {
            Ok(new_thread) => new_thread,
            Err(error) => {
                if let Some(startup_guard) = startup_guard {
                    startup_guard.cleanup().await;
                }
                return Err(error);
            }
        };
        new_thread.thread.emit_thread_ready_lifecycle().await;
        if source_changed_during_startup.load(Ordering::Acquire) {
            new_thread.thread.session.request_mcp_runtime_refresh();
        }
        if is_resumed_thread {
            new_thread.thread.emit_thread_resume_lifecycle().await;
        }
        startup_state.release_membership();
        if let Some(startup_guard) = startup_guard {
            startup_guard.disarm();
        }
        Ok(new_thread)
    }

    async fn finalize_thread_spawn(
        &self,
        session: Arc<Session>,
        io: SessionIo,
        session_source: SessionSource,
        initial_host_config: Option<AgentConfigUpdate>,
    ) -> CodexResult<NewThread> {
        let thread_id = session.thread_id();
        let event = io.next_event().await?;
        let session_configured = match event {
            Event {
                id,
                msg: EventMsg::SessionConfigured(session_configured),
            } if id == INITIAL_SUBMIT_ID => session_configured,
            _ => {
                return Err(CodexErr::SessionConfiguredNotFirstEvent);
            }
        };

        {
            let mut threads = self.threads.write().await;
            if let std::collections::hash_map::Entry::Vacant(e) = threads.entry(thread_id) {
                if let Some(update) = initial_host_config {
                    session
                        .services
                        .agent_control
                        .propagate_config_update(update);
                }
                let thread = Arc::new(CodexThread::new(
                    session,
                    io,
                    ThreadStartupMetadata::from(&session_configured),
                    session_source,
                ));
                e.insert(thread.clone());
                return Ok(NewThread {
                    thread_id,
                    thread,
                    session_configured,
                });
            }
        }

        if let Err(err) = io.shutdown_and_wait().await {
            warn!("failed to shut down duplicate thread {thread_id}: {err}");
        }
        Err(CodexErr::InvalidRequest(format!(
            "thread {thread_id} is already running"
        )))
    }

    pub(crate) fn notify_thread_created(&self, thread_id: ThreadId) {
        let _ = self.thread_created_tx.send(thread_id);
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotTurnState {
    ends_mid_turn: bool,
    active_turn_id: Option<String>,
    active_turn_started_at: Option<i64>,
    active_turn_root_id: Option<String>,
    active_turn_start_index: Option<usize>,
}

fn snapshot_turn_state(history: &InitialHistory) -> SnapshotTurnState {
    let rollout_items = history.get_rollout_items();
    // The last explicitly started turn that has not completed or aborted is still running.
    let mut active_turn: Option<(usize, &codex_protocol::protocol::TurnStartedEvent)> = None;
    for (index, item) in rollout_items.iter().enumerate() {
        match item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(started)) => {
                active_turn = Some((index, started));
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) => {
                active_turn = None;
            }
            _ => {}
        }
    }
    if let Some((start_index, started)) = active_turn {
        return SnapshotTurnState {
            ends_mid_turn: true,
            active_turn_id: Some(started.turn_id.clone()),
            active_turn_started_at: started.started_at,
            active_turn_root_id: started.root_turn_id.clone(),
            active_turn_start_index: Some(start_index),
        };
    }

    let Some(last_user_position) = user_message_positions_in_rollout(rollout_items)
        .last()
        .copied()
    else {
        return SnapshotTurnState {
            ends_mid_turn: false,
            active_turn_id: None,
            active_turn_started_at: None,
            active_turn_root_id: None,
            active_turn_start_index: None,
        };
    };

    // Synthetic fork/resume histories can contain user/assistant response items
    // without explicit turn lifecycle events. If the persisted snapshot has no
    // terminating boundary after its last user message, treat it as mid-turn.
    SnapshotTurnState {
        ends_mid_turn: !rollout_items[last_user_position + 1..].iter().any(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
            )
        }),
        active_turn_id: None,
        active_turn_started_at: None,
        active_turn_root_id: None,
        active_turn_start_index: None,
    }
}

/// Fork the current history as if the source thread had been interrupted now.
///
/// If the history ends mid-turn, this appends the same `<turn_aborted>` marker produced by a
/// real interrupt. Otherwise the history is returned unchanged.
pub(crate) fn fork_history_as_interrupted(
    history: InitialHistory,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let snapshot_state = snapshot_turn_state(&history);
    let history = match history {
        InitialHistory::New => InitialHistory::New,
        InitialHistory::Cleared => InitialHistory::Cleared,
        InitialHistory::Forked(history) => InitialHistory::Forked(history),
        InitialHistory::Resumed(resumed) => {
            InitialHistory::Forked(Arc::unwrap_or_clone(resumed.history))
        }
    };
    if snapshot_state.ends_mid_turn {
        append_interrupted_boundary(
            history,
            snapshot_state.active_turn_id,
            snapshot_state.active_turn_root_id,
            snapshot_state.active_turn_started_at,
            interrupted_marker,
        )
    } else {
        history
    }
}

/// Append the same persisted interrupt boundary used by the live interrupt path
/// to an existing fork snapshot after the source thread has been confirmed to
/// be mid-turn.
fn append_interrupted_boundary(
    history: InitialHistory,
    turn_id: Option<String>,
    root_turn_id: Option<String>,
    started_at: Option<i64>,
    interrupted_marker: InterruptedTurnHistoryMarker,
) -> InitialHistory {
    let aborted_event = RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
        root_turn_id,
        turn_id,
        reason: TurnAbortReason::Interrupted,
        error: None,
        started_at,
        completed_at: None,
        duration_ms: None,
    }));

    match history {
        InitialHistory::New | InitialHistory::Cleared => {
            let mut history = Vec::new();
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Forked(mut history) => {
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
        InitialHistory::Resumed(resumed) => {
            let mut history = Arc::unwrap_or_clone(resumed.history);
            if let Some(marker) = interrupted_turn_history_marker(interrupted_marker) {
                history.push(RolloutItem::ResponseItem(marker.into()));
            }
            history.push(aborted_event);
            InitialHistory::Forked(history)
        }
    }
}

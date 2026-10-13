use super::input_queue::InputQueue;
use super::mcp_refresh::McpRefresh;
use super::step_context::StepContext;
use super::step_settings::ModelInfoOverrides;
use super::step_settings::StepSettings;
use super::step_settings::StepSettingsUpdate;
use super::*;
use crate::agent::api::AgentControl;
use crate::agents_md_manager::AgentsMdManager;
use crate::agents_md_manager::SessionInstructions;
use crate::environment_selection::ThreadEnvironments;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::hook_mcp_executor::CoreHookMcpExecutor;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::shell_snapshot::ShellSnapshot;
use crate::state::ActiveTurn;
use crate::turn_metadata::ExecutionMetadata;
use codex_extension_api::ExtensionDataInit;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider::SharedModelProvider;
use codex_prompts::render_model_instructions;
use codex_protocol::SessionId;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_skills::SkillError;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_utils_path::replace_path_and_deduplicate;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

/// Context for an initialized model agent
///
/// A session has at most 1 running task at a time, and can be interrupted by user input.
pub(crate) struct Session {
    pub(crate) thread_id: ThreadId,
    pub(crate) installation_id: String,
    pub(super) tx_event: Sender<Event>,
    pub(super) agent_status: watch::Sender<AgentStatus>,
    pub(super) state: Mutex<SessionState>,
    /// Orders accepted settings commits and their persisted events with compaction checkpoints.
    /// Keep this separate from `state` so storage I/O does not block runtime state access.
    pub(super) thread_settings_persistence: Semaphore,
    /// The set of enabled features should be invariant for the lifetime of the
    /// session.
    pub(super) features: Features,
    pub(super) isolation: codex_extension_api::SessionIsolation,
    pub(crate) tool_policy: Arc<codex_extension_api::ToolPolicy>,
    pub(super) multi_agent_version: OnceLock<MultiAgentVersion>,
    /// Owns invalidation and serializes refreshes without blocking captured calls.
    pub(super) mcp_refresh: McpRefresh,
    pub(super) mcp_elicitation_lifecycle_handle: OnceLock<codex_mcp::ElicitationLifecycle>,
    pub(super) mcp_prewarm_tx: async_channel::Sender<()>,
    pub(super) mcp_prewarm_shutdown: CancellationToken,
    pub(super) mcp_prewarm_task: std::sync::Mutex<Option<JoinHandle<()>>>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    pub(crate) async_hook_results: async_channel::Receiver<HookCompletedEvent>,
    pub(crate) input_queue: InputQueue,
    pub(crate) services: SessionServices,
    pub(super) git_enrichment_policy: GitEnrichmentPolicy,
    pub(super) history_initialization: HistoryInitialization,
    pub(super) forked_from_ordinal_exclusive: Option<u64>,
    pub(super) next_internal_sub_id: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct SessionConfiguration {
    /// Runtime provider and its provider-specific execution policy.
    pub(super) provider: SharedModelProvider,

    /// Desired configured inputs inherited by future turns.
    pub(super) step_settings: Arc<StepSettings>,
    /// Host-supplied extension data inherited by future turns.
    pub(super) turn_extension_init: ExtensionDataInit,
    /// Ordered environments inherited by future turns.
    pub(super) environments: Vec<TurnEnvironmentSelection>,
    /// Explicit startup overrides used when resolving effective model metadata.
    pub(super) model_info_overrides: ModelInfoOverrides,

    /// Developer instructions that supplement the base instructions.
    pub(super) developer_instructions: Option<String>,

    /// Base instructions for the session.
    pub(super) base_instructions: String,

    pub(super) allow_login_shell: bool,
    pub(super) shell_environment_policy: ShellEnvironmentPolicy,

    /// Legacy thread cwd used when a turn does not select an environment.
    pub(super) legacy_fallback_cwd: AbsolutePathBuf,
    /// Top-level runtime workspace roots, independent of explicit environment selections.
    pub(super) runtime_workspace_roots: Vec<AbsolutePathBuf>,
    /// Directory containing all Codex state for this session.
    pub(super) codex_home: AbsolutePathBuf,

    // TODO(pakrym): Remove config from here
    pub(super) original_config_do_not_use: Arc<Config>,
    /// Optional service name tag for session metrics.
    pub(super) metrics_service_name: Option<String>,
    pub(super) app_server_client_name: Option<String>,
    pub(super) app_server_client_version: Option<String>,
    /// Source of the session (cli, vscode, exec, mcp, ...)
    pub(super) session_source: SessionSource,
    /// Persisted thread history contract selected when this thread was created.
    pub(super) history_mode: ThreadHistoryMode,
    /// Immediate history source copied into this thread, when this thread was forked.
    pub(super) forked_from_thread_id: Option<ThreadId>,
    /// Immediate control/spawn parent for this thread, when it has one.
    pub(super) parent_thread_id: Option<ThreadId>,
    /// Optional analytics source classification for this thread.
    pub(super) thread_source: Option<ThreadSource>,
    /// Effective originator used for this thread's Responses requests and analytics events.
    pub(super) originator: String,
    pub(super) dynamic_tools: Vec<DynamicToolSpec>,
    pub(super) user_shell_override: Option<shell::Shell>,
}

impl SessionConfiguration {
    pub(super) fn cwd(&self) -> &AbsolutePathBuf {
        &self.legacy_fallback_cwd
    }

    pub(super) fn inferred_environment_config(&self) -> EnvironmentConfig {
        EnvironmentConfig {
            allow_login_shell: self.allow_login_shell,
            workspace_roots: Vec::new(),
            shell_environment_policy: self.shell_environment_policy.clone(),
            selected_capability_roots: Vec::new(),
        }
    }

    pub(super) fn thread_config_snapshot(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> ThreadConfigSnapshot {
        let workspace_roots =
            ThreadEnvironments::primary_workspace_roots_for(&environment_selections);
        ThreadConfigSnapshot {
            model: self.step_settings.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.step_settings.service_tier.clone(),
            environments: TurnEnvironmentSelections::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections,
            ),
            workspace_roots,
            reasoning_effort: self.step_settings.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.step_settings.reasoning_summary,
            personality: self.step_settings.personality,
            collaboration_mode: self.step_settings.collaboration_mode.clone(),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            forked_from_thread_id: self.forked_from_thread_id,
            parent_thread_id: self.parent_thread_id,
            thread_source: self.thread_source.clone(),
            originator: self.originator.clone(),
            turn_extension_init: self.turn_extension_init.clone(),
        }
    }

    /// Captures thread-owned settings for persistence and resume.
    pub(super) fn thread_settings_snapshot(&self) -> ThreadSettingsSnapshot {
        ThreadSettingsSnapshot {
            model: self.step_settings.collaboration_mode.model().to_string(),
            model_provider_id: self.original_config_do_not_use.model_provider_id.clone(),
            service_tier: self.step_settings.service_tier.clone(),
            cwd: self.legacy_fallback_cwd.clone(),
            runtime_workspace_roots: Some(self.runtime_workspace_roots.clone()),
            reasoning_effort: self.step_settings.collaboration_mode.reasoning_effort(),
            reasoning_summary: self.step_settings.reasoning_summary,
            personality: self.step_settings.personality,
            collaboration_mode: self.step_settings.collaboration_mode.clone(),
        }
    }

    /// Captures thread-owned settings and their separately owned environments.
    pub(super) fn restorable_thread_settings(
        &self,
        environment_selections: Vec<TurnEnvironmentSelection>,
    ) -> CodexThreadSettingsOverrides {
        CodexThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                self.legacy_fallback_cwd.clone(),
                environment_selections
                    .into_iter()
                    .map(TurnEnvironmentSelection::into_request)
                    .collect(),
            )),
            runtime_workspace_roots: Some(self.runtime_workspace_roots.clone()),
            summary: self.step_settings.reasoning_summary,
            service_tier: Some(self.step_settings.service_tier.clone()),
            collaboration_mode: Some(self.step_settings.collaboration_mode.clone()),
            personality: self.step_settings.personality,
            turn_extension_init: Some(self.turn_extension_init.clone()),
            ..Default::default()
        }
    }

    pub(super) fn validate(&self, environments: &[TurnEnvironmentSelection]) -> CodexResult<()> {
        super::environment::validate_environment_configs(environments)
    }

    pub(super) fn apply(
        &self,
        updates: &SessionSettingsUpdate,
        current_environments: &[TurnEnvironmentSelection],
    ) -> CodexResult<Self> {
        let mut next_configuration = self.clone();
        if let Some(turn_extension_init) = &updates.turn_extension_init {
            next_configuration.turn_extension_init = turn_extension_init.clone();
        }

        let current_cwd = self.cwd().clone();
        if let Some(environments) = &updates.environments {
            next_configuration.legacy_fallback_cwd = environments.legacy_fallback_cwd.clone();
            next_configuration.environments = environments.environments.clone();
        }
        let cwd_changed = next_configuration.legacy_fallback_cwd != current_cwd;
        if let Some(runtime_workspace_roots) = &updates.runtime_workspace_roots {
            next_configuration.runtime_workspace_roots = runtime_workspace_roots.clone();
        } else if cwd_changed {
            next_configuration.runtime_workspace_roots = replace_path_and_deduplicate(
                next_configuration.runtime_workspace_roots,
                current_cwd.as_path(),
                next_configuration.legacy_fallback_cwd.clone(),
            );
        }
        if let Some(app_server_client_name) = updates.app_server_client_name.clone() {
            next_configuration.app_server_client_name = Some(app_server_client_name);
        }
        if let Some(app_server_client_version) = updates.app_server_client_version.clone() {
            next_configuration.app_server_client_version = Some(app_server_client_version);
        }
        let next_environments = updates
            .environments
            .as_ref()
            .map_or(current_environments, |environments| {
                environments.environments.as_slice()
            });
        super::environment::validate_environment_configs(next_environments)?;
        next_configuration.step_settings =
            Arc::new(self.step_settings.apply(&updates.step_settings));
        Ok(next_configuration)
    }
}

/// The configuration and public snapshot published by one settings commit.
pub(crate) struct SessionSettingsCommit {
    pub(crate) configuration: SessionConfiguration,
    pub(crate) snapshot: ThreadSettingsSnapshot,
}

#[derive(Default, Clone)]
pub(crate) struct SessionSettingsUpdate {
    pub(crate) step_settings: StepSettingsUpdate,
    /// Omission preserves the current data; an empty initializer clears it.
    pub(crate) turn_extension_init: Option<ExtensionDataInit>,
    pub(crate) environments: Option<TurnEnvironmentSelections>,
    pub(crate) runtime_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    pub(crate) service_tier_for_turn: Option<String>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) app_server_client_version: Option<String>,
}

async fn warm_skills_for_session_init(
    config: Arc<Config>,
    skills_service: Arc<HostSkillsService>,
    turn_environments: &TurnEnvironmentSnapshot,
    extensions: &codex_extension_api::ExtensionRegistry<Config>,
) -> Vec<SkillError> {
    if config.features.enabled(Feature::SkipHostSkillDiscovery)
        && !extensions.requires_host_skill_discovery()
    {
        return Vec::new();
    }

    let fs = turn_environments.primary_filesystem();
    let skills_input = skills_load_input_from_config(config.as_ref());
    skills_service
        .snapshot_for_config(&skills_input, fs)
        .await
        .outcome()
        .errors
        .clone()
}

impl Session {
    /// Returns the concrete identity for this thread.
    pub(crate) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// Returns the identity shared by the root thread and all descendant threads.
    pub(crate) fn session_id(&self) -> SessionId {
        self.services.agent_control.identity()
    }

    pub(crate) async fn dynamic_tools(&self) -> Vec<DynamicToolSpec> {
        let state = self.state.lock().await;
        state.session_configuration.dynamic_tools.clone()
    }

    pub(crate) async fn responses_metadata(
        &self,
        step_context: &StepContext,
        request_kind: CodexResponsesRequestKind,
    ) -> CodexResponsesMetadata {
        let (window_id, window_number, context_window_id) = self.current_window().await;
        let mut responses_metadata = step_context.turn.turn_metadata_state.to_responses_metadata(
            self.installation_id.clone(),
            window_id,
            request_kind,
        );
        ExecutionMetadata::from_settings(&step_context.settings).apply_to(&mut responses_metadata);
        responses_metadata.tool_namespaces_info = if step_context
            .turn
            .config
            .tool_registry
            .turn_metadata_includes_tool_info
            && step_context.settings.model_info.use_responses_lite
        {
            step_context.tool_router.tool_namespaces_info().cloned()
        } else {
            None
        };
        self.with_window_and_fork_metadata(
            &step_context.turn,
            responses_metadata,
            window_number,
            context_window_id,
        )
    }

    // TODO(CDXENT-454): Build the compaction request and metadata from the captured execution.
    // Remote compaction currently attaches only finalized tool inventory because the rest of the
    // request remains turn-backed; local compaction does not have a finalized request inventory.
    pub(crate) async fn compaction_responses_metadata(
        &self,
        turn_context: &TurnContext,
        compaction_metadata: CompactionTurnMetadata,
    ) -> CodexResponsesMetadata {
        let (window_id, window_number, context_window_id) = self.current_window().await;
        let responses_metadata = turn_context.turn_metadata_state.to_responses_metadata(
            self.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        );
        self.with_window_and_fork_metadata(
            turn_context,
            responses_metadata,
            window_number,
            context_window_id,
        )
    }

    pub(crate) fn with_window_and_fork_metadata(
        &self,
        turn_context: &TurnContext,
        responses_metadata: CodexResponsesMetadata,
        window_number: u64,
        context_window_id: uuid::Uuid,
    ) -> CodexResponsesMetadata {
        CodexResponsesMetadata {
            history_initialization: Some(self.history_initialization),
            window_number: Some(window_number),
            context_window_id: Some(context_window_id),
            mcp_attribution: Some(self.services.executed_tool_calls.mcp_attribution_snapshot()),
            history_ingest_requested: turn_context
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                .then_some(true),
            forked_from_ordinal_exclusive: self
                .forked_from_ordinal_exclusive
                .filter(|_| responses_metadata.forked_from_thread_id.is_some()),
            ..responses_metadata
        }
    }

    #[instrument(name = "session_init", level = "info", skip_all)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        startup: Option<Arc<super::startup::SessionStartup>>,
        mut session_configuration: SessionConfiguration,
        environment_selections: &[TurnEnvironmentSelection],
        config: Arc<Config>,
        mut instructions: SessionInstructions,
        installation_id: String,
        auth_manager: Arc<AuthManager>,
        models_manager: SharedModelsManager,
        git_root_discovery: Arc<GitRootDiscovery>,
        model_info: ModelInfo,
        tx_event: Sender<Event>,
        agent_status: watch::Sender<AgentStatus>,
        mut initial_history: InitialHistory,
        history_initialization: HistoryInitialization,
        session_source: SessionSource,
        skills_service: Arc<HostSkillsService>,
        mcp_manager: Arc<McpManager>,
        extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
        mut thread_extension_init: ExtensionDataInit,
        client_mcp_extensions: ClientMcpExtensions,
        agent_control: AgentControlInit,
        reserved_thread_id: Option<ThreadId>,
        environment_manager: Arc<EnvironmentManager>,
        inherited_environments: Option<TurnEnvironmentSnapshot>,
        thread_store: Arc<ThreadStore>,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        external_time_provider: Option<Arc<dyn TimeProvider>>,
        multi_agent_version: Option<MultiAgentVersion>,
        git_enrichment_policy: GitEnrichmentPolicy,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            "Configuring session: model={}; provider={:?}",
            session_configuration
                .step_settings
                .collaboration_mode
                .model(),
            session_configuration.provider
        );
        let base_instructions_provenance = if config.base_instructions.is_some() {
            Some(
                config
                    .base_instructions_provenance
                    .clone()
                    .unwrap_or(BaseInstructionsProvenance::Custom),
            )
        } else if let Some(inherited_base_instructions) = initial_history.get_base_instructions() {
            let BaseInstructions { text, provenance } = inherited_base_instructions;
            provenance.or_else(|| {
                (text == render_model_instructions(&model_info)).then(|| {
                    BaseInstructionsProvenance::Model {
                        model: model_info.slug.clone(),
                    }
                })
            })
        } else {
            Some(BaseInstructionsProvenance::Model {
                model: model_info.slug.clone(),
            })
        };
        let forked_from_id = session_configuration
            .forked_from_thread_id
            .or_else(|| initial_history.forked_from_id());
        session_configuration.forked_from_thread_id = forked_from_id;
        let forked_from_ordinal_exclusive = match &initial_history {
            // The resumed thread's canonical SessionMeta comes first. Never inspect inherited
            // metadata: an ancestor's history_base describes a different fork boundary.
            InitialHistory::Resumed(resumed) => {
                resumed.history.first().and_then(|item| match item {
                    RolloutItem::SessionMeta(line) if line.meta.id == resumed.conversation_id => {
                        let meta = &line.meta;
                        let parent_id = meta.forked_from_id?;
                        meta.forked_from_ordinal_exclusive.or_else(|| {
                            meta.history_base
                                .filter(|base| base.thread_id == parent_id)
                                .map(|base| base.end_ordinal_exclusive)
                        })
                    }
                    _ => None,
                })
            }
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        }
        .filter(|_| forked_from_id.is_some());
        let parent_thread_id = session_configuration
            .parent_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id());
        session_configuration.parent_thread_id = parent_thread_id;
        let is_paginated_subagent = matches!(
            session_configuration.history_mode,
            ThreadHistoryMode::Paginated
        ) && matches!(
            session_configuration.thread_source.as_ref(),
            Some(ThreadSource::Subagent)
        );
        if let InitialHistory::Forked(items) = &mut initial_history {
            Self::assign_missing_rollout_response_item_ids(items);
        }
        let multi_agent_version = multi_agent_version.map(OnceLock::from).unwrap_or_default();
        let initial_multi_agent_version = multi_agent_version.get().copied();

        let thread_id = match (&initial_history, reserved_thread_id) {
            (
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_),
                Some(thread_id),
            ) => thread_id,
            (InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_), None) => {
                agent_control.runtime().generate_thread_id()
            }
            (InitialHistory::Resumed(resumed_history), None) => resumed_history.conversation_id,
            (InitialHistory::Resumed(_), Some(_)) => {
                return Err(anyhow::anyhow!(
                    "reserved thread ID cannot be used when resuming a thread"
                ));
            }
        };
        let isolation = thread_extension_init
            .get::<codex_extension_api::SessionIsolation>()
            .map(|policy| *policy)
            .unwrap_or_default();
        if !session_configuration.session_source.is_non_root_agent()
            && isolation != codex_extension_api::SessionIsolation::Isolated
        {
            instructions.thread_provider = agent_control
                .runtime()
                .root_thread_instructions_provider(thread_id, instructions.thread_provider);
        }
        let resumed_session_id = match &initial_history {
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.session_id),
                    _ => None,
                })
            }
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        // Legacy subagent rollouts synthesized session_id from their own thread ID.
        let resumed_session_id = resumed_session_id.filter(|session_id| {
            !session_configuration.session_source.is_non_root_agent()
                || *session_id != SessionId::from(thread_id)
        });
        // session_id is equal to the root thread's ID.
        let session_id = resumed_session_id.unwrap_or_else(|| {
            if session_configuration.session_source.is_non_root_agent() {
                agent_control.control().identity()
            } else {
                SessionId::from(thread_id)
            }
        });
        let initial_auto_compact_window_ids = AutoCompactWindowIds::new_initial();
        let restore_child_window = matches!(&initial_history, InitialHistory::Forked(_))
            && session_configuration.session_source.is_non_root_agent()
            && config.features.enabled(Feature::TokenBudget);
        if restore_child_window && let InitialHistory::Forked(items) = &mut initial_history {
            let child_window_id = initial_auto_compact_window_ids.window_id.to_string();
            for item in items {
                if let RolloutItem::Compacted(checkpoint) = item {
                    checkpoint.window_number = Some(0);
                    checkpoint.first_window_id = Some(child_window_id.clone());
                    checkpoint.previous_window_id = None;
                    checkpoint.window_id = Some(child_window_id.clone());
                }
            }
        }
        let (agent_control, local_agent_runtime): (Arc<dyn AgentControl>, _) = match agent_control {
            AgentControlInit::Local(control) => {
                let control = control.with_session_id(
                    session_id,
                    config
                        .effective_agent_max_threads(MultiAgentVersion::V2)
                        .unwrap_or(usize::MAX),
                );
                if parent_thread_id.is_none() {
                    control.propagate_config_update(AgentConfigUpdate::ServiceTier(
                        session_configuration
                            .step_settings
                            .service_tier
                            .clone()
                            .or_else(|| config.service_tier.clone()),
                    ));
                }
                let runtime = control.runtime.clone();
                (Arc::new(control), runtime)
            }
            AgentControlInit::Provided { control, runtime } => {
                let controller_id = control.identity();
                if controller_id != session_id {
                    return Err(CodexErr::InvalidRequest(format!(
                        "agent controller identity {controller_id} does not match session identity {session_id}"
                    ))
                    .into());
                }
                (control, runtime)
            }
        };
        let time_provider = crate::current_time::resolve_time_provider(
            config.current_time_reminder.as_ref(),
            external_time_provider,
        )?;
        let selected_capability_roots = thread_extension_init
            .get::<Vec<SelectedCapabilityRoot>>()
            .map(|roots| roots.as_ref().clone())
            .unwrap_or_default();
        thread_extension_init.insert(codex_extension_api::ThreadOriginator(
            session_configuration.originator.clone(),
        ));
        // Publish the already resolved model before extensions make startup decisions.
        // Turn construction refreshes this attachment when the selected model changes.
        thread_extension_init.insert(model_info);
        let tool_policy = thread_extension_init
            .get::<codex_extension_api::ToolPolicy>()
            .unwrap_or_default();
        let thread_extension_data = codex_extension_api::ExtensionData::new_with_init(
            thread_id.to_string(),
            thread_extension_init,
        );
        // Kick off independent async setup tasks in parallel to reduce startup latency.
        //
        // - initialize thread persistence with new or resumed session info
        // - perform default shell discovery
        // - load history metadata (skipped for subagents)
        let thread_persistence_fut = async {
            let mut local_guard = LiveThreadInitGuard::default();
            let mut resume_context = None;
            let mut managed_guard = match &startup {
                Some(startup) => Some(startup.persistence.lock().await),
                None => None,
            };
            let guard = managed_guard.as_deref_mut().unwrap_or(&mut local_guard);
            let live_thread = match &initial_history {
                InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => {
                    let params = CreateThreadParams {
                        session_id,
                        thread_id,
                        forked_from_id,
                        parent_thread_id,
                        source: session_source,
                        thread_source: session_configuration.thread_source.clone(),
                        originator: session_configuration.originator.clone(),
                        base_instructions: BaseInstructions {
                            text: session_configuration.base_instructions.clone(),
                            provenance: base_instructions_provenance.clone(),
                        },
                        dynamic_tools: session_configuration.dynamic_tools.clone(),
                        selected_capability_roots: selected_capability_roots.clone(),
                        multi_agent_version: initial_multi_agent_version,
                        history_mode: session_configuration.history_mode,
                        subagent_history_start_ordinal: None,
                        initial_window_id: initial_auto_compact_window_ids.window_id.to_string(),
                        runtime_workspace_roots: Some(config.workspace_roots.clone()),
                        cwd: config.cwd.to_path_buf(),
                        model_provider: config.model_provider_id.clone(),
                    };
                    if is_paginated_subagent && let InitialHistory::Forked(items) = &initial_history
                    {
                        LiveThread::create_with_inherited_model_context(
                            Arc::clone(&thread_store),
                            params,
                            items,
                            guard,
                        )
                        .await?
                    } else {
                        guard
                            .acquire(LiveThread::create(Arc::clone(&thread_store), params))
                            .await?
                    }
                }
                InitialHistory::Resumed(resumed_history) => {
                    let params = ResumeThreadParams {
                        thread_id: resumed_history.conversation_id,
                        history: Some(resumed_history.history.clone()),
                    };
                    let store = Arc::clone(&thread_store);
                    let (context_tx, context_rx) = tokio::sync::oneshot::channel();
                    let live_thread = guard
                        .acquire(async move {
                            let (live_thread, history) = LiveThread::resume(store, params).await?;
                            let _ = context_tx.send(history);
                            Ok(live_thread)
                        })
                        .await?;
                    resume_context = Some(context_rx.await?);
                    live_thread
                }
            };
            Ok::<_, anyhow::Error>((Some(live_thread), local_guard, resume_context))
        }
        .instrument(info_span!(
            "session_init.thread_persistence",
            otel.name = "session_init.thread_persistence",
        ));
        let mcp_auth = auth_manager.auth();

        let mut mcp_auth_changes = auth_manager.auth_change_receiver();
        let config_for_mcp = Arc::clone(&config);
        let mcp_manager_for_mcp = Arc::clone(&mcp_manager);
        let mcp_runtime_cwd = environment_selections
            .first()
            .and_then(|environment| environment.cwd.to_abs_path().ok())
            .map(|cwd| cwd.to_path_buf())
            .unwrap_or_else(|| session_configuration.cwd().to_path_buf());
        let auth_and_mcp_fut = async move {
            let auth = mcp_auth.await;
            let mcp_config = mcp_manager_for_mcp
                .runtime_config_for_step(&config_for_mcp, environment_selections);
            (auth, mcp_config)
        }
        .instrument(info_span!(
            "session_init.auth_mcp",
            otel.name = "session_init.auth_mcp",
        ));

        // Join all independent futures.
        let (thread_persistence_result, (auth, mcp_config)) =
            tokio::join!(thread_persistence_fut, auth_and_mcp_fut);

        let (live_thread, mut live_thread_init, resume_context) = thread_persistence_result
            .map_err(|e| {
                error!("failed to initialize thread persistence: {e:#}");
                e
            })?;
        let session_result: anyhow::Result<Arc<Self>> = async {
            if let InitialHistory::Resumed(resumed) = &mut initial_history
                && let Some(history) = resume_context
            {
                resumed.history = history;
            }
            let mut post_session_configured_events = Vec::<Event>::new();

            for usage in config.features.legacy_feature_usages() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::DeprecationNotice(DeprecationNoticeEvent {
                        summary: usage.summary.clone(),
                        details: usage.details.clone(),
                    }),
                });
            }
            for message in &config.startup_warnings {
                post_session_configured_events.push(Event {
                    id: "".to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: message.clone(),
                    }),
                });
            }
            let effective_config = config.config_layer_stack.effective_config();
            let config_path = config.codex_home.join(CONFIG_TOML_FILE);
            if let Some(event) = unstable_features_warning_event(
                effective_config
                    .get("features")
                    .and_then(TomlValue::as_table),
                config.suppress_unstable_features_warning,
                &config.features,
                &config_path.display().to_string(),
            ) {
                post_session_configured_events.push(event);
            }
            let telemetry_auth = auth.as_ref();
            let auth_mode = telemetry_auth
                .map(CodexAuth::auth_mode)
                .map(TelemetryAuthMode::from);
            let account_id = telemetry_auth.and_then(CodexAuth::get_account_id);
            let account_email = telemetry_auth.and_then(CodexAuth::get_account_email);
            let originator = session_configuration.originator.clone();
            let terminal_type = user_agent();
            let session_model = session_configuration
                .step_settings
                .collaboration_mode
                .model()
                .to_string();
            let auth_env_telemetry = collect_auth_env_telemetry(
                session_configuration.provider.info(),
                auth_manager.codex_api_key_env_enabled(),
            );
            let mut session_telemetry = SessionTelemetry::new(
                thread_id,
                session_model.as_str(),
                session_model.as_str(),
                account_id.clone(),
                account_email.clone(),
                auth_mode,
                originator.clone(),
                config.otel.log_user_prompt,
                terminal_type.clone(),
                session_configuration.session_source.clone(),
            )
            .with_auth_env(auth_env_telemetry.to_otel_metadata())
            .with_user_id(telemetry_auth.and_then(CodexAuth::get_chatgpt_user_id))
            .with_tool_result_log_config(config.otel.tool_result);
            if let Some(metrics) = thread_extension_data.get::<codex_otel::MetricsClient>() {
                session_telemetry = session_telemetry.with_metrics(metrics.as_ref().clone());
            }
            if let Some(service_name) = session_configuration.metrics_service_name.as_deref() {
                session_telemetry = session_telemetry.with_metrics_service_name(service_name);
            }
            crate::config::emit_session_start_metrics(config.as_ref(), &session_telemetry);
            let is_worktree = session_configuration
                .cwd()
                .canonicalize()
                .ok()
                .and_then(|cwd| {
                    codex_git_utils::repository_identity(&cwd).and_then(|_| {
                        get_git_repo_root(&cwd).map(|root| root.join(".git").is_file())
                    })
                });
            let is_worktree_tag = match is_worktree {
                Some(true) => "true",
                Some(false) => "false",
                None => "unknown",
            };
            let is_git_tag = if get_git_repo_root(session_configuration.cwd()).is_some() {
                "true"
            } else {
                "false"
            };
            session_telemetry.counter(
                THREAD_STARTED_METRIC,
                /*inc*/ 1,
                &[("is_git", is_git_tag), ("is_worktree", is_worktree_tag)],
            );

            let mcp_server_names = codex_mcp::effective_mcp_servers(&mcp_config)
                .into_iter()
                .filter_map(|(name, server)| server.enabled().then_some(name))
                .collect::<Vec<_>>();
            session_telemetry.conversation_starts(
                config.model_provider.name.as_str(),
                session_configuration
                    .step_settings
                    .collaboration_mode
                    .reasoning_effort(),
                config
                    .model_reasoning_summary
                    .unwrap_or(ReasoningSummaryConfig::Auto),
                config.model_context_window,
                config.model_auto_compact_token_limit,
                mcp_server_names.iter().map(String::as_str).collect(),
            );

            let default_shell = session_configuration
                .user_shell_override
                .clone()
                .unwrap_or_else(shell::default_user_shell);
            let prefer_executor_shell_snapshots = config.features.enabled(Feature::ShellSnapshotV2)
                && config.features.enabled(Feature::ShellTool);
            let shell_snapshot = if config.features.enabled(Feature::ShellSnapshot)
                && !prefer_executor_shell_snapshots
            {
                ShellSnapshot::new(
                    config.codex_home.clone(),
                    thread_id,
                    session_telemetry.clone(),
                )
            } else {
                ShellSnapshot::disabled()
            };
            let inherited_environments = inherited_environments.unwrap_or_default();
            let turn_environments = Arc::new(ThreadEnvironments::new(
                environment_manager,
                default_shell.clone(),
                ThreadEnvironmentDefaults::new(session_configuration.inferred_environment_config()),
                shell_snapshot,
                inherited_environments.clone(),
                config.features.enabled(Feature::DeferredExecutor),
            ));
            turn_environments.update_selections(environment_selections);
            session_configuration.environments = turn_environments.selections();
            let resolved_environments = turn_environments.snapshot().await;
            let agents_md_manager = Arc::new(AgentsMdManager::new(instructions));
            let skill_warmup = warm_skills_for_session_init(
                Arc::clone(&config),
                Arc::clone(&skills_service),
                &resolved_environments,
                extensions.as_ref(),
            )
            .instrument(info_span!(
                "session_init.skill_warmup",
                otel.name = "session_init.skill_warmup",
            ));
            let (instruction_refresh, skill_errors) = tokio::join!(
                agents_md_manager.refresh(config.as_ref(), &resolved_environments),
                skill_warmup,
            );
            let (agents_md_result, instruction_warnings) = instruction_refresh;
            // TODO(anp): Present AGENTS.md discovery errors more clearly to the user.
            agents_md_result?;
            post_session_configured_events.extend(instruction_warnings.into_iter().map(
                |message| Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent { message }),
                },
            ));
            for err in &skill_errors {
                error!(
                    "failed to load skill {}: {}",
                    err.path.display(),
                    err.message
                );
            }
            let mut state = SessionState::new_with_auto_compact_window_ids(
                session_configuration.clone(),
                initial_auto_compact_window_ids,
                ContextManager::for_session(&session_configuration.session_source),
            );
            state.base_instructions_provenance = base_instructions_provenance.clone();
            // Restore a provided effort baseline before startup prewarm can establish a new one.
            if let Some(pin) = thread_extension_data.remove::<ReasoningEffortPin>() {
                state.reasoning_effort_pin = pin.as_ref().clone();
            }

            // Hooks and extensions share one stable thread-owned MCP runtime handle.
            let mcp_runtime = Arc::new(McpRuntime::empty(mcp_config.prefix_mcp_tool_names));
            let hooks_config =
                build_hooks_config(&config, resolved_environments.single_local_environment());
            let (hooks, async_hook_results) = Hooks::new(
                hooks_config,
                thread_id,
                Arc::new(CoreHookMcpExecutor {
                    runtime: Arc::clone(&mcp_runtime),
                    thread_id,
                }),
            );
            for warning in hooks.startup_warnings() {
                post_session_configured_events.push(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent {
                        message: warning.clone(),
                    }),
                });
            }

            let session_extension_data =
                codex_extension_api::ExtensionData::new(session_id.to_string());
            session_extension_data.insert(session_telemetry.clone());
            let extension_metrics =
                extension_metrics::from_session_telemetry(session_telemetry.clone());
            let workspace_routing =
                thread_extension_data.get_or_init(|| config.workspace_routing_context());
            for contributor in extensions.thread_lifecycle_contributors() {
                contributor
                    .on_thread_start(codex_extension_api::ThreadStartInput {
                        config: config.as_ref(),
                        session_source: &session_configuration.session_source,
                        environments: environment_selections,
                        extension_metrics: Some(Arc::clone(&extension_metrics)),
                        session_store: &session_extension_data,
                        thread_store: &thread_extension_data,
                    })
                    .await;
            }

            let executed_tool_calls =
                crate::state::ExecutedToolCalls::new(&config.features, &initial_history);
            let codex_responses_headers =
                thread_extension_data.get::<crate::CodexResponsesHeaders>();
            let reasoning_effort_override_enabled =
                config.features.enabled(Feature::ReasoningEffortOverride);
            let services = SessionServices {
                // Start with an empty connection set. The initialized set is
                // published after SessionConfigured so MCP events follow it.
                mcp_runtime,
                mcp_handler_cache: Default::default(),
                unified_exec_manager: UnifiedExecProcessManager::new(
                    config.background_terminal_max_timeout,
                ),
                elicitations: crate::elicitation::ElicitationService::new(),
                hooks: arc_swap::ArcSwap::from_pointee(hooks),
                user_shell: Arc::new(default_shell),
                show_raw_agent_reasoning: config.show_raw_agent_reasoning,
                auth_manager: Arc::clone(&auth_manager),
                session_telemetry,
                models_manager: Arc::clone(&models_manager),
                git_root_discovery,
                runtime_handle: tokio::runtime::Handle::current(),
                skills_service,
                agents_md_manager,
                mcp_manager: Arc::clone(&mcp_manager),
                extensions: Arc::clone(&extensions),
                // TODO(jif): extract session to share between sub-agents
                session_extension_data,
                thread_extension_data,
                selected_capability_roots,
                client_mcp_extensions,
                local_agent_runtime,
                agent_control,
                live_thread: live_thread.clone(),
                thread_store: Arc::clone(&thread_store),
                attestation_provider: attestation_provider.clone(),
                time_provider,
                model_client: ModelClient::new(
                    Some(Arc::clone(&auth_manager)),
                    if config.features.enabled(Feature::UseAgentIdentity) {
                        AgentIdentityAuthPolicy::ChatGptAuth
                    } else {
                        AgentIdentityAuthPolicy::JwtOnly
                    },
                    thread_id,
                    session_configuration.provider.info().clone(),
                    session_configuration.session_source.clone(),
                    session_configuration.originator.clone(),
                    config.model_verbosity,
                    config.features.enabled(Feature::ContentItemKinds),
                    reasoning_effort_override_enabled,
                    config.features.enabled(Feature::EnableRequestCompression),
                    config.features.enabled(Feature::RuntimeMetrics),
                    Self::build_model_client_beta_features_header(config.as_ref()),
                    /*concurrent_reasoning_summaries_enabled*/
                    config
                        .features
                        .enabled(Feature::ConcurrentReasoningSummaries),
                    config.features.enabled(Feature::OutputTokenReplay),
                    attestation_provider,
                    config.http_client_factory(),
                    workspace_routing.as_ref().clone(),
                    extensions.model_request_contributors().to_vec(),
                )
                .with_restored_history(matches!(
                    &initial_history,
                    InitialHistory::Resumed(_) | InitialHistory::Forked(_)
                ))
                .with_session_context(
                    /*fork_cache_key*/ None,
                    tx_event.clone(),
                    codex_responses_headers,
                    crate::cyber_access_program::ApiKeyCyberAccessPrograms::from_config(&config),
                ),
                executed_tool_calls,
                tool_search_handler_cache: Default::default(),
                turn_environments: Arc::clone(&turn_environments),
            };
            let (mcp_prewarm_tx, mcp_prewarm_rx) = async_channel::bounded(1);
            let sess = Arc::new(Session {
                thread_id,
                installation_id,
                tx_event: tx_event.clone(),
                agent_status,
                state: Mutex::new(state),
                thread_settings_persistence: Semaphore::new(/*permits*/ 1),
                features: config.features.clone(),
                isolation,
                tool_policy,
                multi_agent_version,
                mcp_refresh: McpRefresh::new(),
                mcp_elicitation_lifecycle_handle: OnceLock::new(),
                mcp_prewarm_tx,
                mcp_prewarm_shutdown: CancellationToken::new(),
                mcp_prewarm_task: std::sync::Mutex::new(None),
                active_turn: Mutex::new(None),
                async_hook_results,
                input_queue: InputQueue::with_controller(
                    thread_id,
                    Arc::clone(&services.agent_control),
                ),
                services,
                git_enrichment_policy,
                history_initialization,
                forked_from_ordinal_exclusive,
                next_internal_sub_id: AtomicU64::new(0),
            });
            if let Some(startup) = &startup {
                startup.set_session(Arc::clone(&sess));
            }
            // Dispatch the SessionConfiguredEvent first and then report any errors.
            let thread_config =
                session_configuration.thread_config_snapshot(turn_environments.selections());
            let events = std::iter::once(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::SessionConfigured(SessionConfiguredEvent {
                    session_id,
                    thread_id,
                    cwd: thread_config.cwd().clone(),
                    forked_from_id: thread_config.forked_from_thread_id,
                    parent_thread_id: thread_config.parent_thread_id,
                    thread_source: thread_config.thread_source,
                    model: thread_config.model,
                    model_provider_id: thread_config.model_provider_id,
                    service_tier: thread_config.service_tier,
                    reasoning_effort: thread_config.reasoning_effort,
                }),
            })
            .chain(post_session_configured_events.into_iter());
            for event in events {
                sess.send_event_raw(event).await;
            }
            turn_environments.start_connection_event_forwarding(tx_event.clone());

            let startup_auth_changed = mcp_auth_changes.has_changed().unwrap_or(false);
            if startup_auth_changed {
                mcp_auth_changes.mark_unchanged();
            }
            let latest_auth = sess.services.auth_manager.auth().await;
            let resolved_environment_selections = resolved_environments.all_selections();
            let mcp_config = if startup_auth_changed
                || mcp_auth_changes.has_changed().unwrap_or(false)
                || resolved_environment_selections.as_slice() != environment_selections
            {
                sess.services
                    .mcp_manager
                    .runtime_config_for_step(config.as_ref(), &resolved_environment_selections)
            } else {
                mcp_config
            };
            sess.install_initial_mcp_runtime(
                &session_configuration,
                latest_auth,
                mcp_config,
                &resolved_environments,
                mcp_runtime_cwd,
            )
            .await?;
            sess.start_mcp_prewarm_worker(mcp_prewarm_rx, mcp_auth_changes);
            sess.follow_inherited_environment_configurations(
                &inherited_environments,
                &session_configuration.environments,
            );
            sess.schedule_startup_prewarm(super::startup_prewarm::PrewarmInput::Base)
                .await;
            let session_start_source = match &initial_history {
                InitialHistory::Forked(_) if forked_from_id.is_some() => {
                    codex_hooks::SessionStartSource::Fork
                }
                // `thread/resume` with supplied history uses `Forked` internally
                // without a fork parent, so it should still report `resume`.
                InitialHistory::Resumed(_) | InitialHistory::Forked(_) => {
                    codex_hooks::SessionStartSource::Resume
                }
                InitialHistory::New => codex_hooks::SessionStartSource::Startup,
                InitialHistory::Cleared => codex_hooks::SessionStartSource::Clear,
            };

            // record_initial_history can emit events. We record only after the SessionConfiguredEvent is emitted.
            Box::pin(sess.record_initial_history(initial_history)).await;
            if restore_child_window {
                sess.state.lock().await.restore_auto_compact_window(
                    /*window_number*/ 0,
                    initial_auto_compact_window_ids,
                );
            }
            {
                let mut state = sess.state.lock().await;
                state.queue_pending_session_start_source(session_start_source);
            }
            Ok(sess)
        }
        .await;
        match session_result {
            Ok(sess) => {
                live_thread_init.commit();
                Ok(sess)
            }
            Err(err) => {
                live_thread_init.discard().await;
                Err(err)
            }
        }
    }
}

use super::step_settings::ResolvedStepSettings;
use super::token_budget::has_explicit_settings;
use super::token_budget::resolve_token_budget;
use super::*;
use crate::config::TokenBudgetConfig;
use crate::cyber_access_program;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::shell_snapshot::ShellSnapshotFile;
use arc_swap::ArcSwap;
use codex_core_plugins::PluginCommandAttribution;
use codex_core_plugins::TrustedPluginRoots;
use codex_exec_server::ExecutorFileSystem;
use codex_file_system::EnvironmentAccess;
use codex_file_system::FileSystemEnvironmentAccessor;
use codex_model_provider::SharedModelProvider;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_skills_extension::HostSkillsSnapshot;
use codex_skills_extension::SkillLoadOutcome;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tracing::instrument;

pub(crate) type ShellSnapshotTask = Shared<BoxFuture<'static, Option<Arc<ShellSnapshotFile>>>>;
#[derive(Clone)]
pub(crate) struct TurnEnvironment {
    pub(crate) selection: TurnEnvironmentSelection,
    pub(crate) config_origin: EnvironmentConfigOrigin,
    pub(crate) environment: Arc<Environment>,
    /// Cached from the selected executor; `None` means it did not report one.
    pub(crate) user_home_dir: Option<PathUri>,
    /// Cached from the selected executor; `None` means it did not report them.
    pub(crate) temporary_directories: Option<Vec<PathUri>>,
    pub(crate) shell: Option<shell::Shell>,
    /// OS reported by the selected executor; `None` for legacy executors.
    pub(crate) executor_platform_os: Option<String>,
    pub(crate) shell_snapshot: ShellSnapshotTask,
    pub(crate) shell_snapshot_v2_supported: bool,
}

impl TurnEnvironment {
    pub(crate) fn new(
        selection: TurnEnvironmentSelection,
        config_origin: EnvironmentConfigOrigin,
        environment: Arc<Environment>,
        shell: Option<shell::Shell>,
    ) -> Self {
        debug_assert!(matches!(selection.config, EnvironmentConfigState::Ready(_)));
        Self {
            selection,
            config_origin,
            environment,
            user_home_dir: None,
            temporary_directories: None,
            shell,
            executor_platform_os: None,
            shell_snapshot: futures::future::ready(None).boxed().shared(),
            shell_snapshot_v2_supported: false,
        }
    }

    pub(crate) fn config(&self) -> &EnvironmentConfig {
        let EnvironmentConfigState::Ready(config) = &self.selection.config else {
            unreachable!("ready turn environments always carry resolved configuration")
        };
        config
    }

    pub(crate) fn shell_environment_policy(&self) -> &ShellEnvironmentPolicy {
        &self.config().shell_environment_policy
    }

    /// Returns the environment's startup snapshot when it matches this login-shell command.
    pub(crate) async fn shell_snapshot(
        &self,
        cwd: &AbsolutePathBuf,
        command: &[String],
        config: &Config,
    ) -> Option<Arc<ShellSnapshotFile>> {
        if !config.features.enabled(Feature::ShellSnapshot)
            || self.selection.cwd != PathUri::from_abs_path(cwd)
            || command.len() < 3
            || command[1] != "-lc"
        {
            return None;
        }
        self.shell_snapshot.peek()?.clone().filter(|snapshot| {
            &snapshot.shell_environment_policy == self.shell_environment_policy()
        })
    }

    pub(crate) fn cwd(&self) -> &PathUri {
        &self.selection.cwd
    }

    pub(crate) fn workspace_roots(&self) -> &[PathUri] {
        &self.config().workspace_roots
    }

    /// Borrows this snapshot's filesystem.
    pub(crate) fn fs_accessor(&self) -> impl EnvironmentAccess + '_ {
        FileSystemEnvironmentAccessor::new(self.environment.filesystem_ref())
    }

    pub(crate) fn selection(&self) -> TurnEnvironmentSelection {
        self.config_origin
            .into_input_selection(self.selection.clone())
    }
}

impl std::fmt::Debug for TurnEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnEnvironment")
            .field("environment_id", &self.selection.environment_id)
            .field("environment", &self.environment)
            .field("cwd", &self.selection.cwd)
            .field("workspace_roots", &self.config().workspace_roots)
            .field("user_home_dir", &self.user_home_dir)
            .field("temporary_directories", &self.temporary_directories)
            .field("shell", &self.shell)
            .field("executor_platform_os", &self.executor_platform_os)
            .field("config", self.config())
            .field("config_origin", &self.config_origin)
            .finish_non_exhaustive()
    }
}

/// Request options for one turn, separate from persistent thread settings.
#[derive(Default)]
pub(crate) struct NewTurnContextOptions {
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) cyber_access_program: Option<CyberAccessProgram>,
}

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) trace_id: Option<String>,
    /// Turn-scoped configuration. Read step-specific settings such as service tier from the
    /// corresponding `StepContext` instead.
    pub config: Arc<Config>,
    /// Preferences captured before token-budget defaults from the turn's initial model.
    pub(crate) configured_token_budget: Option<TokenBudgetConfig>,
    /// Captured once so later steps do not re-read config layers to detect user preferences.
    pub(crate) use_model_token_budget_defaults: bool,
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    /// Frozen settings used to construct this context. Legacy turn consumers
    /// keep this view even when later steps use different settings.
    pub(crate) initial_settings: Arc<ResolvedStepSettings>,
    /// Thread-owned plugin selection captured when this turn was admitted.
    pub(crate) disabled_plugin_ids: Vec<String>,
    /// Settings for the next step; environments are owned by `ThreadEnvironments`.
    pub(super) next_step_settings: ArcSwap<ResolvedStepSettings>,
    /// Turn-wide telemetry; model-attributed step work should use `StepContext::session_telemetry`.
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) provider: SharedModelProvider,
    pub(crate) session_source: SessionSource,
    pub(crate) history_mode: ThreadHistoryMode,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) originator: String,
    /// Initial selection retained for legacy turn consumers. Step work uses StepContext.
    // TODO(sayan): Migrate remaining step consumers to their StepContext's environments.
    pub(crate) initial_environments: TurnEnvironmentSnapshot,
    /// The session's absolute working directory. All relative paths provided
    /// by the model are resolved against this path instead of `std::env::current_dir()`.
    #[deprecated(note = "use the selected turn environment cwd instead")]
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) current_date: Option<String>,
    pub(crate) timezone: Option<String>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) multi_agent_version: MultiAgentVersion,
    pub(crate) available_models: Vec<ModelPreset>,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) turn_metadata_state: Arc<TurnMetadataState>,
    pub(crate) extension_data: Arc<codex_extension_api::ExtensionData>,
    pub(crate) turn_timing_state: Arc<TurnTimingState>,
    pub(crate) terminal_error: Arc<Mutex<Option<ErrorEvent>>>,
    pub(crate) server_model_warning_emitted: AtomicBool,
    pub(crate) model_verification_emitted: AtomicBool,
    /// Effective cyber treatment for this turn, including any child-agent inheritance.
    pub(crate) cyber_access_program: Option<CyberAccessProgram>,
}

/// Selects which preparation is needed when building a turn context.
#[derive(Debug)]
enum TurnContextBuildMode {
    /// Resolves and stores model/multi-agent metadata and performs normal skill discovery.
    /// Used for execution and initial context creation.
    Full,

    /// Prepares startup resources with normal skill discovery, without updating
    /// shared model/multi-agent metadata.
    StartupPrewarm,

    /// Captures recording settings without updating shared model/multi-agent metadata
    /// or discovering skills. Must not initialize context or capture an execution step.
    InjectItems,
}

impl TurnContext {
    /// Returns the resolved causal root for this admitted turn.
    pub(crate) fn root_turn_id(&self) -> String {
        self.turn_metadata_state
            .root_turn_id()
            .unwrap_or_else(|| self.sub_id.clone())
    }

    /// Builds a review turn with fresh turn-local state.
    pub(super) fn for_review(
        &self,
        sub_id: String,
        config: Arc<Config>,
        step_settings: Arc<ResolvedStepSettings>,
        available_models: Vec<ModelPreset>,
        turn_metadata_state: Arc<TurnMetadataState>,
    ) -> Self {
        let session_telemetry = step_settings.telemetry(&self.session_telemetry);
        let extension_data = Arc::new(codex_extension_api::ExtensionData::new(sub_id.clone()));
        extension_data.insert(self.skills_snapshot().as_ref().clone());

        Self {
            sub_id,
            trace_id: current_span_trace_id(),
            configured_token_budget: config.token_budget.clone(),
            use_model_token_budget_defaults: config.features.enabled(Feature::TokenBudget)
                && !has_explicit_settings(&config),
            config,
            auth_manager: self.auth_manager.clone(),
            initial_settings: Arc::clone(&step_settings),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
            next_step_settings: ArcSwap::from(step_settings),
            session_telemetry,
            provider: self.provider.clone(),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            parent_thread_id: self.parent_thread_id,
            originator: self.originator.clone(),
            initial_environments: self.initial_environments.clone(),
            available_models,
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            app_server_client_name: self.app_server_client_name.clone(),
            developer_instructions: None,
            multi_agent_version: MultiAgentVersion::Disabled,
            #[allow(deprecated)]
            cwd: self.cwd.clone(),
            final_output_json_schema: None,
            dynamic_tools: self.dynamic_tools.clone(),
            turn_metadata_state,
            extension_data,
            turn_timing_state: Arc::new(TurnTimingState::default()),
            terminal_error: Arc::new(Mutex::new(None)),
            server_model_warning_emitted: AtomicBool::new(false),
            model_verification_emitted: AtomicBool::new(false),
            cyber_access_program: None,
        }
    }

    pub(crate) fn attribution(&self) -> codex_history::TurnAttribution {
        codex_history::TurnAttribution {
            turn_id: self.sub_id.clone(),
            turn_trigger: self.turn_metadata_state.current_turn_trigger(),
            parent_turn_id: self.turn_metadata_state.parent_turn_id(),
            initiating_agent_path: self.turn_metadata_state.initiating_agent_path().cloned(),
            root_turn_id: self.turn_metadata_state.root_turn_id(),
        }
    }

    /// Captures current model metadata without preparing a step.
    pub(crate) fn capture_current_model_info(&self) -> Arc<ModelInfo> {
        Arc::clone(&self.next_step_settings.load().model_info)
    }

    /// Legacy: returns the frozen initial-turn model metadata.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn model_info(&self) -> &Arc<ModelInfo> {
        &self.initial_settings.model_info
    }

    /// Legacy: returns the frozen initial-turn reasoning-effort selection.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn reasoning_effort(&self) -> Option<&ReasoningEffortConfig> {
        self.initial_settings.reasoning_effort()
    }

    /// Legacy: returns the frozen initial-turn effective reasoning summary.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn reasoning_summary(&self) -> ReasoningSummaryConfig {
        self.initial_settings.reasoning_summary
    }

    /// Legacy: returns the frozen initial-turn collaboration mode kind.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn mode(&self) -> ModeKind {
        self.initial_settings.selected_collaboration_mode().mode
    }

    pub(crate) fn skills_snapshot(&self) -> Arc<HostSkillsSnapshot> {
        let Some(snapshot) = self.extension_data.get::<HostSkillsSnapshot>() else {
            unreachable!("every turn has a host skills snapshot");
        };
        snapshot
    }

    /// Legacy: returns the frozen initial-turn collaboration mode with the resolved model slug.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn collaboration_mode(&self) -> CollaborationMode {
        self.initial_settings.effective_collaboration_mode()
    }

    pub(crate) fn plugin_attribution_for_command(
        &self,
        command: &[String],
        cwd: &AbsolutePathBuf,
    ) -> Option<PluginCommandAttribution> {
        self.extension_data
            .get::<TrustedPluginRoots>()?
            .resolve_attribution(command, cwd)
    }

    pub(crate) async fn plugin_attribution_for_executor_command(
        &self,
        command: &[String],
        cwd: &PathUri,
        file_system: &dyn ExecutorFileSystem,
    ) -> Option<PluginCommandAttribution> {
        self.extension_data
            .get::<TrustedPluginRoots>()?
            .resolve_executor_attribution(command, cwd, file_system)
            .await
    }

    /// Legacy: returns the frozen initial-turn reasoning effort, including the initial model default.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.initial_settings.effective_reasoning_effort()
    }

    /// Legacy: returns the frozen initial-turn reasoning-effort label for tracing.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn effective_reasoning_effort_for_tracing(&self) -> String {
        self.effective_reasoning_effort()
            .map(|effort| effort.to_string())
            .unwrap_or_else(|| "default".to_string())
    }

    /// Legacy: returns the frozen initial-turn model context window.
    /// Step-scoped consumers should use their captured `StepContext::settings`.
    pub(crate) fn model_context_window(&self) -> Option<i64> {
        self.model_info().usable_context_window()
    }

    pub(crate) fn apps_enabled(&self) -> bool {
        let uses_codex_backend = self
            .auth_manager
            .as_deref()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        self.config
            .features
            .apps_enabled_for_auth(uses_codex_backend)
            && self.config.orchestrator_mcp_enabled
    }

    pub(crate) async fn with_model(
        &self,
        model: String,
        models_manager: &SharedModelsManager,
    ) -> Self {
        let mut config = (*self.config).clone();
        config.model = Some(model.clone());
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let supported_reasoning_levels = model_info
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort.clone())
            .collect::<Vec<_>>();
        let reasoning_effort = if let Some(current_reasoning_effort) = self.reasoning_effort() {
            if supported_reasoning_levels.contains(current_reasoning_effort) {
                Some(current_reasoning_effort.clone())
            } else {
                supported_reasoning_levels
                    .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                    .cloned()
                    .or_else(|| model_info.default_reasoning_level.clone())
            }
        } else {
            supported_reasoning_levels
                .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                .cloned()
                .or_else(|| model_info.default_reasoning_level.clone())
        };
        config.model_reasoning_effort = reasoning_effort.clone();

        let available_models = models_manager
            .list_models(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await;
        let model_info = Arc::new(model_info);
        let mut selected = self.initial_settings.selected().clone();
        selected.collaboration_mode = selected.collaboration_mode.with_updates(
            Some(model),
            Some(reasoning_effort),
            /*developer_instructions*/ None,
        );
        let step_settings = Arc::new(ResolvedStepSettings::new(
            Arc::new(selected),
            model_info,
            &config.features,
        ));
        config.service_tier = step_settings.service_tier.clone();
        let session_telemetry = step_settings.telemetry(&self.session_telemetry);

        Self {
            sub_id: self.sub_id.clone(),
            trace_id: self.trace_id.clone(),
            config: Arc::new(config),
            configured_token_budget: self.configured_token_budget.clone(),
            use_model_token_budget_defaults: self.use_model_token_budget_defaults,
            auth_manager: self.auth_manager.clone(),
            initial_settings: Arc::clone(&step_settings),
            disabled_plugin_ids: self.disabled_plugin_ids.clone(),
            next_step_settings: ArcSwap::from(step_settings),
            session_telemetry,
            provider: self.provider.clone(),
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            parent_thread_id: self.parent_thread_id,
            originator: self.originator.clone(),
            initial_environments: self.initial_environments.clone(),
            #[allow(deprecated)]
            cwd: self.cwd.clone(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            app_server_client_name: self.app_server_client_name.clone(),
            developer_instructions: self.developer_instructions.clone(),
            multi_agent_version: self.multi_agent_version,
            available_models,
            final_output_json_schema: self.final_output_json_schema.clone(),
            dynamic_tools: self.dynamic_tools.clone(),
            turn_metadata_state: self.turn_metadata_state.clone(),
            extension_data: Arc::clone(&self.extension_data),
            turn_timing_state: Arc::clone(&self.turn_timing_state),
            terminal_error: Arc::clone(&self.terminal_error),
            server_model_warning_emitted: AtomicBool::new(
                self.server_model_warning_emitted.load(Ordering::Relaxed),
            ),
            model_verification_emitted: AtomicBool::new(
                self.model_verification_emitted.load(Ordering::Relaxed),
            ),
            cyber_access_program: self.cyber_access_program,
        }
    }

    pub(crate) fn to_turn_context_item(&self) -> TurnContextItem {
        #[allow(deprecated)]
        let cwd = self.cwd.clone();
        TurnContextItem {
            turn_id: Some(self.sub_id.clone()),
            root_turn_id: self.turn_metadata_state.root_turn_id(),
            disabled_plugin_ids: Some(self.disabled_plugin_ids.clone()),
            cwd,
            model: self.model_info().slug.clone(),
            comp_hash: self.model_info().comp_hash.clone(),
            collaboration_mode: Some(self.collaboration_mode()),
            multi_agent_version: Some(self.multi_agent_version),
            cyber_access_program: self.cyber_access_program,
            effort: self.reasoning_effort().cloned(),
            summary: Some(ReasoningSummaryConfig::None),
        }
    }
}

fn local_time_context() -> (String, String) {
    match iana_time_zone::get_timezone() {
        Ok(timezone) => (Local::now().format("%Y-%m-%d").to_string(), timezone),
        Err(_) => (
            Utc::now().format("%Y-%m-%d").to_string(),
            "Etc/UTC".to_string(),
        ),
    }
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    pub(crate) fn build_per_turn_config(
        &self,
        session_configuration: &SessionConfiguration,
        cwd: AbsolutePathBuf,
        workspace_roots: Vec<AbsolutePathBuf>,
    ) -> Config {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.cwd = cwd;
        per_turn_config.workspace_roots = workspace_roots;
        per_turn_config.model_reasoning_effort = session_configuration
            .step_settings
            .collaboration_mode
            .reasoning_effort();
        per_turn_config.model_reasoning_summary =
            session_configuration.step_settings.reasoning_summary;
        per_turn_config.service_tier = session_configuration.step_settings.service_tier.clone();
        per_turn_config.personality = session_configuration.step_settings.personality;
        per_turn_config.web_search_mode = resolve_web_search_mode_for_turn(
            per_turn_config.web_search_mode,
            session_configuration.provider.capabilities(),
        );
        per_turn_config.features = config.features.clone();
        per_turn_config
    }

    pub(crate) fn build_effective_session_config(
        &self,
        session_configuration: &SessionConfiguration,
    ) -> Config {
        let workspace_roots =
            crate::environment_selection::ThreadEnvironments::primary_workspace_roots_for(
                &session_configuration.environments,
            );
        let mut config = self.build_per_turn_config(
            session_configuration,
            session_configuration.cwd().clone(),
            workspace_roots,
        );
        config.model = Some(
            session_configuration
                .step_settings
                .collaboration_mode
                .model()
                .to_string(),
        );
        config
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "turn_context.make", level = "trace", skip_all)]
    pub(crate) fn make_turn_context(
        thread_id: ThreadId,
        session_id: SessionId,
        auth_manager: Option<Arc<AuthManager>>,
        session_telemetry: &SessionTelemetry,
        provider: SharedModelProvider,
        session_configuration: &SessionConfiguration,
        multi_agent_version: MultiAgentVersion,
        per_turn_config: Config,
        step_settings: Arc<ResolvedStepSettings>,
        models_manager: &SharedModelsManager,
        environments: TurnEnvironmentSnapshot,
        cwd: AbsolutePathBuf,
        sub_id: String,
        skills_snapshot: HostSkillsSnapshot,
    ) -> TurnContext {
        let model_info = &step_settings.model_info;
        let session_telemetry_for_context = step_settings.telemetry(session_telemetry);
        let session_source = session_configuration.session_source.clone();
        let available_models = models_manager.try_list_models().unwrap_or_default();

        let mut per_turn_config = per_turn_config;
        let configured_token_budget = per_turn_config.token_budget.clone();
        let use_model_token_budget_defaults =
            per_turn_config.features.enabled(Feature::TokenBudget)
                && !has_explicit_settings(&per_turn_config);
        per_turn_config.token_budget = resolve_token_budget(
            configured_token_budget.as_ref(),
            use_model_token_budget_defaults,
            model_info,
        );
        if per_turn_config
            .features
            .persistent_execution_enabled(step_settings.reasoning_effort())
        {
            super::time_reminder::apply_persistent_defaults(&mut per_turn_config);
        }
        per_turn_config.service_tier = step_settings.service_tier.clone();
        let per_turn_config = Arc::new(per_turn_config);
        let turn_metadata_state = Arc::new(TurnMetadataState::new(
            session_id.to_string(),
            thread_id.to_string(),
            session_configuration.forked_from_thread_id,
            session_configuration.parent_thread_id,
            &session_configuration.session_source,
            session_configuration.thread_source.clone(),
            sub_id.clone(),
            cwd.clone(),
        ));
        turn_metadata_state
            .set_responses_api_metadata(per_turn_config.responses_api_metadata.clone());
        let (current_date, timezone) = local_time_context();
        let extension_data = Arc::new(codex_extension_api::ExtensionData::new_with_init(
            sub_id.clone(),
            session_configuration.turn_extension_init.clone(),
        ));
        extension_data.insert(skills_snapshot);
        TurnContext {
            sub_id,
            trace_id: current_span_trace_id(),
            config: per_turn_config,
            configured_token_budget,
            use_model_token_budget_defaults,
            auth_manager,
            initial_settings: Arc::clone(&step_settings),
            disabled_plugin_ids: session_configuration.disabled_plugin_ids.clone(),
            next_step_settings: ArcSwap::from(step_settings),
            session_telemetry: session_telemetry_for_context,
            provider,
            session_source,
            history_mode: session_configuration.history_mode,
            parent_thread_id: session_configuration.parent_thread_id,
            originator: session_configuration.originator.clone(),
            initial_environments: environments,
            #[allow(deprecated)]
            cwd,
            current_date: Some(current_date),
            timezone: Some(timezone),
            app_server_client_name: session_configuration.app_server_client_name.clone(),
            developer_instructions: session_configuration.developer_instructions.clone(),
            multi_agent_version,
            available_models,
            final_output_json_schema: None,
            dynamic_tools: session_configuration.dynamic_tools.clone(),
            turn_metadata_state,
            extension_data,
            turn_timing_state: Arc::new(TurnTimingState::default()),
            terminal_error: Arc::new(Mutex::new(None)),
            server_model_warning_emitted: AtomicBool::new(false),
            model_verification_emitted: AtomicBool::new(false),
            cyber_access_program: None,
        }
    }

    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
        options: NewTurnContextOptions,
    ) -> CodexResult<(Arc<TurnContext>, ThreadSettingsSnapshot)> {
        let Some((turn_context, snapshot)) = self
            .new_turn_with_sub_id_if(sub_id, updates, options, |_, _| true)
            .await?
        else {
            unreachable!("unconditional turn construction must accept valid settings");
        };
        Ok((turn_context, snapshot))
    }

    /// Commits accepted settings atomically, then constructs the turn without holding
    /// the state lock. The caller owns admission policy.
    ///
    /// `should_start` runs under the state lock against the current and validated
    /// proposed configurations. It must be fast and side-effect-free, and must not
    /// block, acquire other locks, or call back into `Session`.
    pub(super) async fn new_turn_with_sub_id_if(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
        options: NewTurnContextOptions,
        should_start: impl FnOnce(&SessionConfiguration, &SessionConfiguration) -> bool + Send,
    ) -> CodexResult<Option<(Arc<TurnContext>, ThreadSettingsSnapshot)>> {
        let service_tier_for_turn = updates.service_tier_for_turn.clone();
        let commit = match self.update_settings_if(updates, should_start).await {
            Ok(Some(commit)) => commit,
            Ok(None) => return Ok(None),
            Err(error) => {
                let message = CodexErr::InvalidRequest(error.to_string()).to_string();
                self.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: message.clone(),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                })
                .await;
                return Err(CodexErr::InvalidRequest(message));
            }
        };
        let mut configuration = commit.configuration;
        // Apply the override only to the turn's copy, after persisting thread settings.
        if let Some(service_tier) = service_tier_for_turn {
            Arc::make_mut(&mut configuration.step_settings).service_tier = Some(service_tier);
        }
        self.services
            .models_manager
            .refresh_after_auth_change(
                self.build_effective_session_config(&configuration)
                    .http_client_factory(),
            )
            .await;
        let turn_environments = self.activate_turn_environments(&configuration).await;
        let turn_context = self
            .new_turn_from_configuration(sub_id, configuration, turn_environments, options)
            .await;
        Ok(Some((turn_context, commit.snapshot)))
    }

    /// Builds a context from the caller's chosen settings and environments without starting work.
    async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        turn_environments: TurnEnvironmentSnapshot,
        options: NewTurnContextOptions,
    ) -> Arc<TurnContext> {
        self.new_turn_context_from_configuration(
            sub_id,
            session_configuration,
            turn_environments,
            options,
            TurnContextBuildMode::Full,
            self.git_enrichment_policy,
        )
        .await
    }

    async fn new_startup_prewarm_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
    ) -> Arc<TurnContext> {
        let turn_environments = self.services.turn_environments.snapshot().await;
        self.new_turn_context_from_configuration(
            sub_id,
            session_configuration,
            turn_environments,
            NewTurnContextOptions::default(),
            TurnContextBuildMode::StartupPrewarm,
            GitEnrichmentPolicy::Skip,
        )
        .await
    }

    #[instrument(name = "turn_context.build", level = "trace", skip_all, fields(?build_mode))]
    async fn new_turn_context_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        turn_environments: TurnEnvironmentSnapshot,
        options: NewTurnContextOptions,
        build_mode: TurnContextBuildMode,
        git_enrichment_policy: GitEnrichmentPolicy,
    ) -> Arc<TurnContext> {
        let primary_turn_environment = turn_environments.primary();
        // TODO(anp): Migrate per-turn config and legacy TurnContext cwd consumers to PathUri so
        // a foreign primary environment does not fall back to the session's host cwd.
        let cwd = primary_turn_environment
            .as_ref()
            .and_then(|turn_environment| turn_environment.cwd().to_abs_path().ok())
            .unwrap_or_else(|| session_configuration.cwd().clone());
        let per_turn_config = self.build_per_turn_config(
            &session_configuration,
            cwd.clone(),
            turn_environments.primary_workspace_roots(),
        );
        let model_info = session_configuration
            .step_settings
            .resolve_model_info(
                self.services.models_manager.as_ref(),
                &session_configuration.model_info_overrides,
            )
            .await;
        let multi_agent_version = match build_mode {
            TurnContextBuildMode::Full => {
                // Only execution and initial context creation publish model metadata.
                self.services
                    .thread_extension_data
                    .insert(model_info.clone());
                self.resolve_multi_agent_version_for_model(&model_info, &per_turn_config)
            }
            TurnContextBuildMode::StartupPrewarm | TurnContextBuildMode::InjectItems => {
                per_turn_config.multi_agent_version_for_model(
                    self.multi_agent_version()
                        .or(model_info.multi_agent_version),
                )
            }
        };
        let plugins_input = per_turn_config.plugins_config_input();
        let plugin_outcome = self
            .services
            .plugins_manager
            .plugins_for_config(&plugins_input)
            .await
            .without_plugins(&session_configuration.disabled_plugin_ids);
        let trusted_plugin_roots = TrustedPluginRoots::from_plugin_load_outcome(
            &plugin_outcome,
            per_turn_config.codex_home.as_path(),
        );
        let skills_snapshot = if matches!(build_mode, TurnContextBuildMode::InjectItems)
            || (per_turn_config
                .features
                .enabled(Feature::SkipHostSkillDiscovery)
                && !self.services.extensions.requires_host_skill_discovery())
        {
            // Executor and orchestrator catalogs are supplied independently of host skills.
            HostSkillsSnapshot::new(Arc::new(SkillLoadOutcome::default()))
        } else {
            let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
            let plugin_skill_snapshots = self
                .services
                .plugins_manager
                .plugin_skill_snapshots_for_config(&plugins_input);
            let skills_input =
                skills_load_input_from_config(&per_turn_config, effective_skill_roots)
                    .with_plugin_skill_snapshots(plugin_skill_snapshots);
            let fs = primary_turn_environment
                .map(|turn_environment| turn_environment.environment.get_filesystem());
            self.services
                .skills_service
                .snapshot_for_config(&skills_input, fs)
                .await
        };
        let step_settings = Arc::new(ResolvedStepSettings::new(
            Arc::clone(&session_configuration.step_settings),
            Arc::new(model_info),
            &self.features,
        ));
        let mut turn_context: TurnContext = Self::make_turn_context(
            self.thread_id(),
            self.session_id(),
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.session_telemetry,
            session_configuration.provider.clone(),
            &session_configuration,
            multi_agent_version,
            per_turn_config,
            step_settings,
            &self.services.models_manager,
            turn_environments,
            cwd,
            sub_id,
            skills_snapshot,
        );
        turn_context.extension_data.insert(trusted_plugin_roots);
        turn_context.final_output_json_schema = options.final_output_json_schema;
        turn_context.cyber_access_program = cyber_access_program::for_provider(
            &turn_context.config.model_provider_id,
            options.cyber_access_program,
        );
        // Tool observers retain this turn's selection even after a later turn starts.
        if let Some(program) = turn_context.cyber_access_program {
            turn_context.extension_data.insert(program);
        }
        let turn_context = Arc::new(turn_context);
        if git_enrichment_policy == GitEnrichmentPolicy::Fresh
            && turn_context
                .initial_environments
                .single_local_environment_cwd()
                .is_some()
        {
            turn_context
                .turn_metadata_state
                .spawn_git_enrichment_task(Arc::clone(&self.services.git_root_discovery));
        }
        turn_context
    }

    pub(crate) async fn maybe_emit_model_warnings_for_turn(&self, tc: &TurnContext) {
        if tc.model_info().used_fallback_model_metadata {
            self.send_event(
                tc,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Model metadata for `{}` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.",
                        tc.model_info().slug
                    ),
                }),
            )
            .await;
        }
    }

    /// Builds a context without starting work or changing the current environments.
    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_for(TurnContextBuildMode::Full).await
    }

    /// Captures current recording settings without discovering skills.
    /// This context must not be used to capture or execute a step.
    pub(crate) async fn new_inject_items_context(&self) -> Arc<TurnContext> {
        self.new_default_turn_for(TurnContextBuildMode::InjectItems)
            .await
    }

    async fn new_default_turn_for(&self, build_mode: TurnContextBuildMode) -> Arc<TurnContext> {
        let session_configuration = self.default_turn_configuration().await;
        let turn_environments = self.services.turn_environments.snapshot().await;
        self.new_turn_context_from_configuration(
            self.next_internal_sub_id(),
            session_configuration,
            turn_environments,
            NewTurnContextOptions::default(),
            build_mode,
            self.git_enrichment_policy,
        )
        .await
    }

    /// Prepares new work on the saved environments; context-only callers keep the current ones.
    pub(crate) async fn new_turn_with_default_settings(
        &self,
        sub_id: String,
        options: NewTurnContextOptions,
    ) -> Arc<TurnContext> {
        let session_configuration = self.default_turn_configuration().await;
        let turn_environments = self
            .activate_turn_environments(&session_configuration)
            .await;
        self.new_turn_from_configuration(sub_id, session_configuration, turn_environments, options)
            .await
    }

    pub(crate) async fn new_startup_prewarm_turn_with_sub_id(
        &self,
        sub_id: String,
    ) -> Arc<TurnContext> {
        let session_configuration = self.default_turn_configuration().await;
        self.new_startup_prewarm_turn_from_configuration(sub_id, session_configuration)
            .await
    }

    async fn default_turn_configuration(&self) -> SessionConfiguration {
        let state = self.state.lock().await;
        state.session_configuration.clone()
    }
}

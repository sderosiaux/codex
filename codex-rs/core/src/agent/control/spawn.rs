use super::residency::is_v2_resident_session_source;
use super::spawn_guard::PendingSpawn;
use super::*;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::role::apply_role_to_config;
use crate::agent::types::AgentMetadata;
use crate::agent::types::LiveAgent;
use crate::agent::types::SpawnAgentOptions;
use crate::agents_md_manager::SessionInstructions;
use crate::codex_thread::CodexThread;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::context::BaseInstructionsFragment;
use crate::context::ContextualUserFragment;
use crate::context::CurrentTimeReminder;
use crate::context::CurrentTimeUnavailable;
use crate::context::DeveloperInstructions;
use crate::context::MultiAgentModeInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::MultiAgentUsageHint;
use crate::context::world_state::IncrementalToolsHint;
use crate::context::world_state::PersistentModeState;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::multi_agents::resolve_usage_hints;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::fork_history_as_interrupted;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_prompts::ResolvedModelMessages;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::protocol::EnvironmentConfigState;

const AGENT_NAMES: &str = include_str!("../../../assets/agent/agent_names.txt");

/// Initial input delivered after a spawned agent acquires execution capacity.
///
/// V2 communication spawns keep the communication and its context paired so centralized
/// submission and lifecycle logging cannot receive one without the other. Other spawn sources
/// provide user input directly, making an uncontextualized inter-agent communication
/// unrepresentable.
#[allow(clippy::large_enum_variant)]
pub(super) enum SpawnInitialInput {
    UserInput(Vec<UserInput>),
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext),
}

fn default_agent_nickname_list() -> Vec<&'static str> {
    AGENT_NAMES
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

pub(super) fn agent_nickname_candidates(config: &Config, role_name: Option<&str>) -> Vec<String> {
    let role_name = role_name.unwrap_or(DEFAULT_ROLE_NAME);
    if let Some(candidates) =
        resolve_role_config(config, role_name).and_then(|role| role.nickname_candidates.clone())
    {
        return candidates;
    }

    default_agent_nickname_list()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

fn keep_forked_rollout_item(item: &RolloutItem, preserve_context_baselines: bool) -> bool {
    match item {
        RolloutItem::ResponseItem(envelope) => match &envelope.item {
            ResponseItem::Message { role, phase, .. } => match role.as_str() {
                "system" | "developer" | "user" => true,
                "assistant" => matches!(
                    phase,
                    Some(MessagePhase::PartialAnswer | MessagePhase::FinalAnswer)
                ),
                _ => false,
            },
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::FunctionCallOutput { call_id: None, .. }
            | ResponseItem::ConfigurationUpdate { .. } => true,
            ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput {
                call_id: Some(_), ..
            }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => false,
        },
        RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::RetainedContext(_) => false,
        // Full-history forks preserve the cached prompt prefix and can keep diffing
        // from the parent's durable baseline unless legacy compaction requires rebuilding it.
        RolloutItem::TurnContext(_) | RolloutItem::WorldState(_) => preserve_context_baselines,
        // Child threads inherit model context, not the parent's cumulative usage state.
        RolloutItem::TokenUsageRecord(_) => false,
        RolloutItem::Compacted(_) | RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => true,
    }
}

fn retain_forked_developer_message(item: &mut ResponseItem, usage_hint_texts: &[String]) -> bool {
    if !matches!(item, ResponseItem::Message { role, .. } if role == "developer") {
        return true;
    }

    let Some(mut content) = to_annotated_content(item) else {
        return false;
    };
    content.retain(|content_item| {
        // Persisted role hints can predate the current bundled wording and lack markers.
        if matches!(
            content_item.kind().as_str(),
            "guardian.approved_action" | "multi_agent.role_instructions" | "multi_agent.usage_hint"
        ) {
            return false;
        }
        let ContentItem::InputText { text } = content_item.content() else {
            return true;
        };

        !(MultiAgentRoleInstructions::matches_text(text)
            || MultiAgentModeInstructions::matches_text(text)
            || CurrentTimeReminder::matches_text(text)
            || CurrentTimeUnavailable::matches_text(text)
            || usage_hint_texts
                .iter()
                .any(|usage_hint_text| usage_hint_text == text))
    });
    !content.is_empty() && set_annotated_content(item, content).is_some()
}

impl LocalAgentControl {
    fn validate_loaded_v2_child(
        &self,
        thread: &CodexThread,
        parent_thread_id: ThreadId,
    ) -> CodexResult<()> {
        if thread.is_running()
            && thread.multi_agent_version() == Some(MultiAgentVersion::V2)
            && thread.session_source.parent_thread_id() == Some(parent_thread_id)
            && Arc::ptr_eq(
                &self.runtime.registry,
                &thread.session.services.local_agent_runtime.registry,
            )
        {
            return Ok(());
        }
        Err(CodexErr::InvalidRequest(format!(
            "multi-agent v2 child {} is not owned by its loaded parent",
            thread.session.thread_id
        )))
    }

    /// A provided parent enables owner-validated reloads; `None` preserves sender-driven reloads.
    /// Keep the returned residency pin until the caller has submitted its input.
    pub(crate) async fn ensure_v2_agent_loaded(
        &self,
        mut config: Config,
        thread_id: ThreadId,
        parent: Option<Arc<CodexThread>>,
    ) -> CodexResult<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        let membership = self.runtime.admit_start()?;
        let state = self.runtime.upgrade()?;
        let owner_thread_id = parent.as_ref().map(|parent| parent.session.thread_id);
        if let Some(parent) = &parent {
            let parent_thread_id = parent.session.thread_id;
            let registered_parent = state.get_thread(parent_thread_id).await.ok();
            if !registered_parent
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, parent))
                || !parent.is_running()
                || parent.multi_agent_version() != Some(MultiAgentVersion::V2)
                || !Arc::ptr_eq(
                    &self.runtime.registry,
                    &parent.session.services.local_agent_runtime.registry,
                )
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id}: parent ownership is unavailable; resume the parent first"
                )));
            }
        }
        if owner_thread_id.is_none()
            && let Ok(thread) = state.get_thread(thread_id).await
        {
            return self.runtime.pin_v2_residency(&state, &thread).await;
        }
        if self
            .runtime
            .registry
            .agent_metadata_for_thread(thread_id)
            .is_none()
        {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let mut environment_selections = self.runtime.registry.evicted_environments(thread_id);

        let stored_thread = state
            .read_stored_thread(thread_id, /*include_history*/ false)
            .await?;
        let stored_model = stored_thread.model.clone();
        let stored_model_provider = stored_thread.model_provider.clone();
        let stored_reasoning_effort = stored_thread.reasoning_effort.clone();
        let stored_source = stored_thread.source.clone();
        let stored_parent_thread_id = stored_thread.parent_thread_id;
        let history = state.load_thread_history(thread_id).await?;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
        });
        if initial_history.get_multi_agent_version() != Some(MultiAgentVersion::V2) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        let (session_source, _) = initial_history
            .get_resumed_session_sources()
            .unwrap_or((stored_source, None));
        if let Some(parent_thread_id) = owner_thread_id {
            if session_source.parent_thread_id() != Some(parent_thread_id)
                || initial_history
                    .get_resumed_parent_thread_id()
                    .is_some_and(|recorded_parent| recorded_parent != parent_thread_id)
                || stored_parent_thread_id
                    .is_some_and(|recorded_parent| recorded_parent != parent_thread_id)
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id}: recorded parent ownership is inconsistent"
                )));
            }
            if let Ok(thread) = state.get_thread(thread_id).await {
                self.validate_loaded_v2_child(&thread, parent_thread_id)?;
                return self.runtime.pin_v2_residency(&state, &thread).await;
            }
        }
        let parent = if let Some(parent) = parent {
            let turn = parent
                .session
                .new_turn_with_default_settings(Uuid::now_v7().to_string(), Default::default())
                .await;
            config = build_agent_resume_config(&turn).map_err(|_| {
                CodexErr::InvalidRequest(format!(
                    "cannot resume multi-agent v2 child {thread_id} with the current parent settings"
                ))
            })?;
            Some((parent, turn.initial_environments.clone()))
        } else {
            None
        };
        config.model_reasoning_effort = stored_reasoning_effort;
        if let Some(role_name) = session_source.get_agent_role() {
            let runtime_cwd = config.cwd.clone();
            apply_role_to_config(&mut config, Some(&role_name))
                .await
                .map_err(CodexErr::InvalidRequest)?;
            config.cwd = runtime_cwd;
        }
        config.service_tier = self.root_service_tier();
        if let Some(model) = stored_model {
            config.model = Some(model);
        }
        if config.model_provider_id != stored_model_provider {
            config.model_provider = config
                .model_providers
                .get(&stored_model_provider)
                .cloned()
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(format!(
                        "Model provider `{stored_model_provider}` not found"
                    ))
                })?;
            config.model_provider_id = stored_model_provider;
        }
        let parent_thread_id = owner_thread_id
            .or_else(|| initial_history.get_resumed_parent_thread_id())
            .or(stored_parent_thread_id);
        let (inherited_environments, client_mcp_extensions) = if let Some((
            parent,
            parent_environments,
        )) = parent.as_ref()
        {
            if let Some(selections) = environment_selections.as_mut() {
                for selection in selections {
                    let environment_id = &selection.environment_id;
                    let invalid_environment = |reason: &str| {
                        CodexErr::InvalidRequest(format!(
                            "cannot resume multi-agent v2 child {thread_id}: cached environment {environment_id} {reason}"
                        ))
                    };
                    // Matching the attachment also keeps startup on the captured owner executor.
                    let owner_environment = parent_environments
                        .turn_environments()
                        .find(|environment| {
                            let parent_selection = &environment.selection;
                            parent_selection.environment_id == selection.environment_id
                                && parent_selection.cwd == selection.cwd
                                && parent_selection.workspace_roots == selection.workspace_roots
                        })
                        .ok_or_else(|| {
                            invalid_environment("no longer matches a ready parent environment")
                        })?;
                    let owner_config = owner_environment.config();
                    let child_config = match &selection.config {
                        EnvironmentConfigState::FromThread => {
                            // Pin current owner authority instead of re-inferring child settings.
                            selection.config = EnvironmentConfigState::Ready(owner_config.clone());
                            continue;
                        }
                        EnvironmentConfigState::Ready(config) => config,
                    };
                    if child_config != owner_config {
                        return Err(invalid_environment(
                            "configuration differs from the current parent",
                        ));
                    }
                }
            }
            (
                Some(parent_environments.clone()),
                Some(parent.client_mcp_extensions()),
            )
        } else {
            (
                self.inherited_environments_for_source(&state, Some(&session_source))
                    .await,
                None,
            )
        };
        let inherited_instructions = if let Some((parent, _)) = parent.as_ref() {
            Some(parent.session.inherited_instructions().await)
        } else if let Some(parent_thread_id) = parent_thread_id
            && let Ok(parent) = state.get_thread(parent_thread_id).await
        {
            Some(parent.session.inherited_instructions().await)
        } else {
            self.runtime
                .shared_thread_instructions_provider
                .get()
                .map(|provider| SessionInstructions {
                    thread_provider: Some(Arc::clone(provider)),
                    ..Default::default()
                })
        };
        // Reserving a slot can evict an idle nested parent. Capture its instructions
        // alongside its authority before the live parent can disappear.
        let residency_slot = self
            .reserve_v2_residency_slot(&state, &config, &membership, Some(thread_id))
            .await?;

        match state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config,
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections,
                inherited_environments,
                inherited_instructions,
                client_mcp_extensions,
            })
            .await
        {
            Ok(reloaded_thread) => {
                if let Some(parent_thread_id) = owner_thread_id {
                    self.validate_loaded_v2_child(&reloaded_thread.thread, parent_thread_id)?;
                }
                let runtime = self.runtime.clone();
                // Finish registration even if the sender is cancelled while acquiring its pin.
                tokio::spawn(async move {
                    let _membership = membership;
                    let residency_pin = runtime
                        .pin_v2_residency(&state, &reloaded_thread.thread)
                        .await?;
                    runtime.registry.clear_evicted_environments(thread_id);
                    residency_slot.commit(reloaded_thread.thread_id);
                    // Register before listeners can forward events from the resumed thread.
                    state.notify_thread_created(reloaded_thread.thread_id);
                    Ok(residency_pin)
                })
                .await?
            }
            Err(err) => {
                if let Ok(thread) = state.get_thread(thread_id).await {
                    if let Some(parent_thread_id) = owner_thread_id {
                        self.validate_loaded_v2_child(&thread, parent_thread_id)?;
                    }
                    self.runtime.registry.clear_evicted_environments(thread_id);
                    drop(residency_slot);
                    return self.runtime.pin_v2_residency(&state, &thread).await;
                }
                Err(err)
            }
        }
    }

    pub(super) async fn spawn_agent_internal(
        &self,
        config: Config,
        initial_input: SpawnInitialInput,
        session_source: Option<SessionSource>,
        options: SpawnAgentOptions,
    ) -> CodexResult<(LiveAgent, ThreadConfigSnapshot)> {
        let membership = self.runtime.admit_start()?;
        let state = self.runtime.upgrade()?;
        // Resolve the parent before inheriting settings or copying attachments.
        // Recovering it later from history only repairs the child's metadata.
        let parent_thread_id = session_source
            .as_ref()
            .and_then(SessionSource::parent_thread_id)
            .or(options.parent_thread_id);
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &InitialHistory::New,
                session_source.as_ref(),
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        if let Some(session_source) = session_source.as_ref() {
            self.ensure_execution_capacity(multi_agent_version, session_source)?;
        }
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let spawn_uses_v2_residency = multi_agent_version == MultiAgentVersion::V2
            && session_source
                .as_ref()
                .is_some_and(is_v2_resident_session_source);
        let residency_slot = if spawn_uses_v2_residency {
            Some(
                self.reserve_v2_residency_slot(
                    &state,
                    &config,
                    &membership,
                    /*protected_thread_id*/ None,
                )
                .await?,
            )
        } else {
            None
        };
        let reservation_max_threads = if spawn_uses_v2_residency {
            None
        } else {
            agent_max_threads
        };
        let mut reservation = self
            .runtime
            .registry
            .reserve_spawn_slot(reservation_max_threads)?;
        let inherited_environments = match &options.environments {
            Some(environments) => Some(environments.clone()),
            None => {
                self.inherited_environments_for_source(&state, session_source.as_ref())
                    .await
            }
        };
        let (session_source, mut agent_metadata) = match session_source {
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role,
                ..
            })) => {
                let (session_source, agent_metadata) = self.prepare_thread_spawn(
                    &mut reservation,
                    &config,
                    parent_thread_id,
                    depth,
                    agent_path,
                    agent_role,
                    /*preferred_agent_nickname*/ None,
                )?;
                (Some(session_source), agent_metadata)
            }
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();

        // Capture child defaults once. Copying conversation history must not change
        // the capabilities of the turn which requested the spawn.
        let mut start_options = StartThreadOptions {
            session_source: session_source.clone(),
            thread_source: session_source.as_ref().map(|_| ThreadSource::Subagent),
            environments: options
                .environments
                .as_ref()
                .map(TurnEnvironmentSnapshot::selections),
            inherited_environments,
            ..StartThreadOptions::new(config.clone())
        };
        match (session_source, options.fork_mode.as_ref()) {
            (Some(session_source), Some(_)) => {
                let (initial_history, history_mode) = self
                    .prepare_forked_history(
                        &state,
                        &session_source,
                        &options,
                        &config,
                        multi_agent_version,
                    )
                    .await
                    .map_err(|err| err.with_agent_context(AgentErrorContext::ForkHistory))?;
                start_options.initial_history = initial_history;
                start_options.history_mode = history_mode;
            }
            (Some(_), None) => {
                if let Some(parent_thread_id) = parent_thread_id
                    && let Ok(parent_thread) = state.get_thread(parent_thread_id).await
                {
                    start_options.history_mode = matches!(
                        parent_thread.config_snapshot().await.history_mode,
                        ThreadHistoryMode::Paginated
                    )
                    .then_some(ThreadHistoryMode::Paginated);
                    if multi_agent_version == MultiAgentVersion::V2
                        && config.features.enabled(Feature::MultiAgentV2DynamicTools)
                    {
                        start_options.dynamic_tools = parent_thread.session.dynamic_tools().await;
                    }
                }
            }
            (None, _) => {}
        }
        let new_thread = state
            .spawn_child_thread(start_options, self.clone(), parent_thread_id)
            .await
            .map_err(|err| err.with_agent_context(AgentErrorContext::ChildStartup))?;
        agent_metadata.agent_id = Some(new_thread.thread_id);
        let pending_spawn = PendingSpawn::new(Arc::clone(&state), new_thread.thread_id, membership);

        // Capture startup settings before the child can publish metadata for a later turn.
        let spawn_config = if multi_agent_version == MultiAgentVersion::V2 {
            let mut config = new_thread.thread.config_snapshot().await;
            let model_info = new_thread
                .thread
                .thread_extension_data()
                .get::<codex_protocol::openai_models::ModelInfo>()
                .ok_or_else(|| {
                    CodexErr::Fatal("spawned thread is missing model metadata".to_string())
                })?;
            config.reasoning_effort = config
                .reasoning_effort
                .or_else(|| model_info.default_reasoning_level.clone())
                .map(|effort| model_info.resolve_reasoning_effort(effort));
            Some(config)
        } else {
            None
        };

        let start_options = TurnStartOptions {
            parent_turn_id: options.parent_turn_id,
            turn_trigger: options.turn_trigger,
            root_turn_id: options.root_turn_id,
            cyber_access_program: options.cyber_access_program,
            ..Default::default()
        };
        match initial_input {
            SpawnInitialInput::UserInput(input) => {
                self.send_input(new_thread.thread_id, input, start_options)
                    .await
                    .map_err(|err| err.with_agent_context(AgentErrorContext::InputAdmission))?;
            }
            SpawnInitialInput::InterAgentCommunication(communication, context) => {
                self.send_inter_agent_communication_after_capacity_check(
                    new_thread.thread_id,
                    &state,
                    communication,
                    context,
                    start_options,
                )
                .await
                .map_err(|err| err.with_agent_context(AgentErrorContext::InputAdmission))?;
            }
        }
        reservation.commit(agent_metadata.clone());
        if let Some(residency_slot) = residency_slot {
            residency_slot.commit(new_thread.thread_id);
        }
        let _membership = pending_spawn.disarm();

        // Notify a new thread has been created. This notification will be processed by clients
        // to subscribe or drain this newly created thread.
        // TODO(jif) add helper for drain
        state.notify_thread_created(new_thread.thread_id);
        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| new_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                new_thread.thread_id,
                notification_source,
                child_reference,
                agent_metadata.agent_path.clone(),
            );
        }

        let agent = LiveAgent {
            thread_id: new_thread.thread_id,
            metadata: agent_metadata,
            status: self.get_status(new_thread.thread_id).await,
        };
        let config = match spawn_config {
            Some(config) => config,
            None => new_thread.thread.config_snapshot().await,
        };
        Ok((agent, config))
    }

    /// Prepare copied history while leaving startup settings and timing with the caller.
    async fn prepare_forked_history(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: &SessionSource,
        options: &SpawnAgentOptions,
        config: &Config,
        multi_agent_version: MultiAgentVersion,
    ) -> CodexResult<(InitialHistory, Option<ThreadHistoryMode>)> {
        if options.fork_parent_spawn_call_id.is_none() {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a parent spawn call id".to_string(),
            ));
        }
        if options.fork_mode.is_none() {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a fork mode".to_string(),
            ));
        }
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = session_source
        else {
            return Err(CodexErr::Fatal(
                "spawn_agent fork requires a thread-spawn session source".to_string(),
            ));
        };

        let parent_thread_id = *parent_thread_id;
        let parent_thread = state.get_thread(parent_thread_id).await?;
        let (subagent_developer_instructions, parent_developer_instructions) = match (
            multi_agent_version,
            config
                .multi_agent_v2
                .subagent_developer_instructions
                .as_ref(),
        ) {
            (MultiAgentVersion::V2, override_instructions)
                if override_instructions.is_some() || session_source.get_agent_role().is_some() =>
            {
                let parent_developer_instructions = match parent_thread
                    .session
                    .new_default_turn()
                    .await
                    .developer_instructions
                    .clone()
                {
                    Some(instructions) if !instructions.is_empty() => Some(instructions),
                    Some(_) | None => None,
                };
                (
                    Some(config.developer_instructions.clone().unwrap_or_default()),
                    parent_developer_instructions,
                )
            }
            (MultiAgentVersion::Disabled | MultiAgentVersion::V1, _)
            | (MultiAgentVersion::V2, _) => (None, None),
        };
        let parent_history_mode = parent_thread.config_snapshot().await.history_mode;
        let destination_history_mode = matches!(parent_history_mode, ThreadHistoryMode::Paginated)
            .then_some(ThreadHistoryMode::Paginated);
        let mut forked_rollout_items = state.load_thread_history(parent_thread_id).await?;

        let multi_agent_v2_usage_hint_texts_to_filter: Vec<String> =
            if multi_agent_version == MultiAgentVersion::V2 {
                let parent_config = parent_thread.session.get_config().await;
                let parent_usage_hints = resolve_usage_hints(
                    &parent_config.multi_agent_v2,
                    ResolvedModelMessages::bundled().multi_agent(),
                    !parent_config.update_plan_enabled,
                );
                [parent_usage_hints.root, parent_usage_hints.subagent]
                    .into_iter()
                    .flatten()
                    .map(|instructions| instructions.render())
                    .collect()
            } else {
                Vec::new()
            };
        let mut preserve_context_baselines = true;
        for item in forked_rollout_items.iter().rev() {
            let RolloutItem::Compacted(compacted) = item else {
                continue;
            };
            // Legacy checkpoints force the child to rebuild context regardless of the
            // live parent's reference baseline; an older superseded checkpoint does not.
            if compacted.replacement_history.is_none() {
                preserve_context_baselines = false;
            }
            break;
        }
        let mut replaced_parent_developer_instructions = false;
        let preserve_fork_prefix = multi_agent_version == MultiAgentVersion::V2
            && config.multi_agent_v2.preserve_fork_prefix
            && preserve_context_baselines;
        if preserve_fork_prefix {
            let InitialHistory::Forked(items) = fork_history_as_interrupted(
                InitialHistory::Forked(forked_rollout_items),
                InterruptedTurnHistoryMarker::Disabled,
            ) else {
                unreachable!("a forked snapshot remains forked");
            };
            forked_rollout_items = items;
        }
        let retain_forked_item = |envelope: &mut ResponseItemEnvelope, replaced: &mut bool| {
            if multi_agent_version == MultiAgentVersion::V2
                && (preserve_fork_prefix
                    || matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user" || role == "assistant"))
            {
                // Resume must not recapture copied parent messages as local authorization.
                envelope
                    .metadata
                    .get_or_insert_default()
                    .inherited_user_message = true;
            }
            if let Some(metadata) = &mut envelope.metadata
                && (metadata.sender_user_messages.take().is_some()
                    || !matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user"))
            {
                // Assistant and tool positions belong to the parent counter, not the child.
                metadata.user_input_order = None;
            }
            if preserve_fork_prefix {
                return true;
            }
            let response_item = &mut envelope.item;
            // Tool declarations and their comparison baseline must survive or be rebuilt together.
            // Apply this to both standalone items and compaction replacement histories.
            if matches!(response_item, ResponseItem::AdditionalTools { .. }) {
                return preserve_context_baselines;
            }
            if matches!(response_item, ResponseItem::AgentMessage { .. }) {
                return false;
            }
            if !retain_forked_developer_message(
                response_item,
                &multi_agent_v2_usage_hint_texts_to_filter,
            ) {
                return false;
            }

            if matches!(response_item, ResponseItem::Message { role, .. } if role == "developer") {
                let Some(mut content) = to_annotated_content(response_item) else {
                    return false;
                };
                content.retain_mut(|content_item| {
                    if matches!(
                        content_item.kind().as_str(),
                        BaseInstructionsFragment::KIND | IncrementalToolsHint::KIND
                    ) {
                        return preserve_context_baselines;
                    }
                    let ContentItem::InputText { text } = content_item.content_mut() else {
                        return true;
                    };
                    if PersistentModeState::matches_text(text) {
                        // If the child will rebuild its initial context, drop the inherited
                        // instructions; startup will add the current effort instructions once.
                        return preserve_context_baselines;
                    }
                    let (
                        Some(parent_developer_instructions),
                        Some(subagent_developer_instructions),
                    ) = (
                        parent_developer_instructions.as_ref(),
                        subagent_developer_instructions.as_ref(),
                    )
                    else {
                        return true;
                    };
                    // TODO(anp) track better message fragment provenance in rollouts.
                    if !text.contains(parent_developer_instructions) {
                        return true;
                    }

                    *replaced = true;
                    let replacement = if preserve_context_baselines {
                        subagent_developer_instructions.as_str()
                    } else {
                        ""
                    };
                    *text = text.replace(parent_developer_instructions, replacement);
                    !text.is_empty()
                });
                return !content.is_empty()
                    && set_annotated_content(response_item, content).is_some();
            }

            true
        };
        forked_rollout_items.retain_mut(|item| {
            let keep = (preserve_fork_prefix && matches!(item, RolloutItem::ResponseItem(_)))
                || keep_forked_rollout_item(item, preserve_context_baselines);
            if !keep
                || destination_history_mode == Some(ThreadHistoryMode::Paginated)
                    && matches!(
                        &*item,
                        RolloutItem::EventMsg(
                            EventMsg::ItemCompleted(_)
                                | EventMsg::TokenCount(_)
                                | EventMsg::ThreadGoalUpdated(_)
                                | EventMsg::ThreadSettingsApplied(_),
                        )
                    )
            {
                return false;
            }

            match item {
                RolloutItem::ResponseItem(response_item) => {
                    retain_forked_item(response_item, &mut replaced_parent_developer_instructions)
                }
                RolloutItem::Compacted(compacted) => {
                    // This compaction becomes part of the subagent's initial history. Rewrite its
                    // metadata to describe the child rather than the parent.
                    compacted.latest_token_usage_record = None;
                    if let Some(resume_metadata) = &mut compacted.resume_metadata {
                        resume_metadata.multi_agent_version = Some(multi_agent_version);
                        resume_metadata.turn_attribution = None;
                        if !preserve_context_baselines {
                            resume_metadata.previous_turn_settings = None;
                        }
                    }
                    // Only V2 fetches root authorization live. Its local scope starts known-empty;
                    // V1 must remain incomplete when inherited authorization has been stripped.
                    compacted.retained_context = (multi_agent_version == MultiAgentVersion::V2)
                        .then(codex_history::RetainedContext::default);
                    if let Some(replacement_history) = compacted.replacement_history.as_mut() {
                        // Matches before this checkpoint cannot survive its replacement history.
                        replaced_parent_developer_instructions = false;
                        replacement_history.retain_mut(|response_item| {
                            retain_forked_item(
                                response_item,
                                &mut replaced_parent_developer_instructions,
                            )
                        });
                    }
                    true
                }
                RolloutItem::WorldState(world_state) => {
                    if multi_agent_version == MultiAgentVersion::V2 {
                        world_state.state.remove("multi_agent_usage_hint");
                    }
                    true
                }
                RolloutItem::EventMsg(_)
                | RolloutItem::SessionMeta(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => true,
                RolloutItem::RetainedContext(_) | RolloutItem::TokenUsageRecord(_) => false,
            }
        });
        if preserve_fork_prefix {
            forked_rollout_items.push(RolloutItem::ResponseItem(
                ContextualUserFragment::into(MultiAgentUsageHint::new(
                    "You are now a newly spawned child agent. The preceding conversation is inherited parent context. Follow your own role, identity, and task below; earlier identities, tasks, and action-specific approvals belong to your ancestors. Inherited tool calls are historical records; live tool sessions and process handles are not inherited. Report your result to your parent.",
                )).into(),
            ));
        }
        // Full forks reuse the parent's reference context instead of rebuilding it. If that
        // context omitted the parent's developer fragment, append the child's override so its
        // instructions still reach the model exactly once.
        if let Some(subagent_developer_instructions) = subagent_developer_instructions.as_ref()
            && preserve_context_baselines
            && !replaced_parent_developer_instructions
            && !subagent_developer_instructions.is_empty()
            && parent_thread
                .session
                .reference_context_item()
                .await
                .is_some()
        {
            let developer_message = ContextualUserFragment::into(DeveloperInstructions::new(
                subagent_developer_instructions,
            ));
            forked_rollout_items.push(RolloutItem::ResponseItem(developer_message.into()));
        }
        if preserve_context_baselines
            && multi_agent_version == MultiAgentVersion::V2
            && let Some(subagent_usage_hint) = options
                .multi_agent_v2_usage_hints
                .as_ref()
                .map(|hints| hints.subagent.clone())
                .unwrap_or_else(|| {
                    resolve_usage_hints(
                        &config.multi_agent_v2,
                        ResolvedModelMessages::bundled().multi_agent(),
                        !config.update_plan_enabled,
                    )
                    .subagent
                })
        {
            let subagent_usage_hint_message = ContextualUserFragment::into(subagent_usage_hint);
            forked_rollout_items.push(RolloutItem::ResponseItem(
                subagent_usage_hint_message.into(),
            ));
        }
        Ok((
            InitialHistory::Forked(forked_rollout_items),
            destination_history_mode,
        ))
    }

    /// Reopen a closed agent thread from the history it recorded in this process.
    pub(crate) async fn reopen_agent(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let _membership = self.runtime.admit_start()?;
        Box::pin(self.resume_single_agent(config, thread_id, session_source)).await
    }

    async fn resume_single_agent(
        &self,
        config: Config,
        thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<ThreadId> {
        let state = self.runtime.upgrade()?;
        let stored_thread = state
            .read_stored_thread(thread_id, /*include_history*/ false)
            .await?;
        let resumed_agent_path = stored_thread
            .agent_path
            .as_deref()
            .map(AgentPath::try_from)
            .transpose()
            .map_err(|err| CodexErr::InvalidRequest(format!("invalid stored agent path: {err}")))?;
        let resumed_agent_nickname = stored_thread.agent_nickname.clone();
        let resumed_agent_role = stored_thread.agent_role.clone();
        let history = state.load_thread_history(thread_id).await?;
        let initial_history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
        });
        let parent_thread_id = stored_thread.parent_thread_id;
        let multi_agent_version = state
            .effective_multi_agent_version_for_spawn(
                &initial_history,
                Some(&session_source),
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                &config,
            )
            .await;
        let agent_max_threads = config.effective_agent_max_threads(multi_agent_version);
        let mut reservation = self
            .runtime
            .registry
            .reserve_spawn_slot(agent_max_threads)?;
        let (session_source, agent_metadata) = match session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                agent_path,
                agent_role: _,
                agent_nickname: _,
            }) => self.prepare_thread_spawn(
                &mut reservation,
                &config,
                parent_thread_id,
                depth,
                agent_path.or(resumed_agent_path),
                resumed_agent_role,
                resumed_agent_nickname,
            )?,
            other => (other, AgentMetadata::default()),
        };
        let notification_source = session_source.clone();
        let inherited_environments = self
            .inherited_environments_for_source(&state, Some(&session_source))
            .await;

        let resumed_thread = state
            .resume_thread_with_history_with_source(ResumeThreadWithHistoryOptions {
                config: config.clone(),
                initial_history,
                agent_control: self.clone(),
                session_source,
                parent_thread_id,
                environment_selections: None,
                inherited_environments,
                inherited_instructions: None,
                client_mcp_extensions: None,
            })
            .await?;
        let mut agent_metadata = agent_metadata;
        agent_metadata.agent_id = Some(resumed_thread.thread_id);
        reservation.commit(agent_metadata.clone());
        // Resumed threads are re-registered in-memory and need the same listener
        // attachment path as freshly spawned threads.
        state.notify_thread_created(resumed_thread.thread_id);
        if multi_agent_version != MultiAgentVersion::V2 {
            let child_reference = agent_metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| resumed_thread.thread_id.to_string());
            self.maybe_start_completion_watcher(
                resumed_thread.thread_id,
                Some(notification_source),
                child_reference,
                agent_metadata.agent_path,
            );
        }
        Ok(resumed_thread.thread_id)
    }
}

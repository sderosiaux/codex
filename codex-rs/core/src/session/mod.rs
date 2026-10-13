pub(crate) mod startup;

use crate::WithTurnExtensionData;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::agent::AgentStatus;
use crate::agent::agent_status_from_event;
use crate::agent::api::AgentConfigUpdate;
use crate::agent::api::AgentTurnOutcome;
use crate::agent::control::AgentControlInit;
use crate::agent::status::is_final;
use crate::agents_md_manager::SessionInstructions;
use crate::attestation::AttestationProvider;
use crate::compact;
use crate::compact::CompactedHistoryMetadata;
use crate::context::ContextualUserFragment;
use crate::context::DeveloperInstructions;
use crate::context::ModelSwitchInstructions;
use crate::context::MultiAgentRoleInstructions;
use crate::context::world_state::Placement;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::context::world_state::WorldStateUpdate;
use crate::context::world_state::WorldStateUpdateContent;
use crate::context::world_state::split_prefix_updates;
use crate::current_time::TimeProvider;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::image_preparation::ImagePreparationMode;
use crate::image_preparation::ImageResizeNoticeMode;
use crate::image_preparation::prepare_response_items as prepare_image_response_items;
use crate::image_preparation::unified_image_budget_enabled;
use crate::parse_turn_item;
use crate::session::step_context::StepContext;
use crate::session::step_settings::ResolvedStepSettings;
use crate::session::step_settings::StepSettings;
use crate::session::turn_context::TurnEnvironment;
use crate::skills_load_input_from_config;
use crate::turn_metadata::TurnMetadataState;
use crate::turn_timing::now_unix_timestamp_ms;
use async_channel::Receiver;
use async_channel::Sender;
use chrono::Local;
use chrono::Utc;
use codex_async_utils::OrCancelExt;
use codex_context_fragments::RenderedFragment;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ConversationHistorySnapshot;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::PromptSlot;
use codex_extension_api::TurnContextContributionInput;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_features::Features;
use codex_features::unstable_features_warning_event;
use codex_history::HistoryInitialization;
use codex_history::RolloutItem;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_mcp::McpRuntime;
use codex_mcp::McpRuntimeContext;
use codex_mcp::McpRuntimeInput;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::auth_storage::AuthStorageOriginator;
use codex_otel::current_span_trace_id;
use codex_otel::current_span_w3c_trace_context;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_prompts::render_model_instructions;
use codex_protocol::ResponseUsageMetadata;
use codex_protocol::ThreadId;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ImageReference;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_terminal_detection::user_agent;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LiveThread;
use codex_thread_store::LiveThreadInitGuard;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadStore;
use codex_thread_store::should_persist_response_item;
use codex_utils_audio::prepare_response_items as prepare_audio_response_items;
use codex_utils_git_discovery::GitRootDiscovery;
use codex_utils_output_truncation::with_serialization_allowance;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::prelude::*;
use rmcp::model::RequestId;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;
use uuid::Uuid;

use crate::client::ModelClient;
use crate::codex_thread::CodexThreadSettingsOverrides;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::config::resolve_web_search_mode_for_turn;
use crate::context_manager::ContextManager;
use crate::context_manager::HistoryReplacement;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerSource;
use codex_config::types::McpServerConfig;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;

mod config_refresh;
pub(crate) mod context_window;
mod environment;
mod extension_interruption;
pub(crate) mod extension_metrics;
mod handlers;
mod inject;
mod reasoning_effort;
mod submission;
pub(crate) use reasoning_effort::RequestEffortUsage;
pub(crate) use submission::Submission;
mod input_queue;
mod mcp;
mod mcp_prewarm;
mod mcp_refresh;
mod mcp_runtime;
pub(crate) mod multi_agents;
mod retained_context;
mod review;
mod rollout_budget;
mod rollout_reconstruction;
#[allow(clippy::module_inception)]
pub(crate) mod session;
pub(crate) mod startup_prewarm;
mod step_activation;
pub(crate) mod step_context;
pub(crate) mod step_settings;
mod thread_settings;
pub(crate) mod time_reminder;
mod token_budget;
pub(crate) mod turn;
pub(crate) mod turn_context;
mod turn_input;
mod world_state;
pub(crate) use self::environment::ThreadEnvironmentDefaults;
use self::handlers::submission_loop;
pub(crate) use self::input_queue::InputQueueActivity;
pub(crate) use self::input_queue::TurnInput;
pub(crate) use self::input_queue::TurnInputQueue;
pub(crate) use self::input_queue::UserInputMetadata;
use self::review::spawn_review_thread;
use self::session::Session;
use self::session::SessionConfiguration;
use self::session::SessionSettingsCommit;
pub(crate) use self::session::SessionSettingsUpdate;
use self::turn_context::TurnContext;

use crate::mcp::McpManager;
use crate::session::startup_prewarm::SessionStartupPrewarmHandle;
use crate::shell;
use crate::state::AutoCompactWindowIds;
use crate::state::AutoCompactWindowSnapshot;
use crate::state::ReasoningEffortPin;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::state::TaskKind;
use crate::tasks::ReviewTask;
use crate::turn_timing::TurnTimingState;
use crate::turn_timing::record_turn_ttfm_metric;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_git_utils::get_git_repo_root;
use codex_history::CodexHarnessMetadata;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::InitialHistory;
pub(crate) use codex_history::PreviousTurnSettings;
use codex_history::ResponseItemEnvelope;
use codex_mcp::McpConfig;
use codex_mcp::effective_mcp_servers;
use codex_otel::SessionTelemetry;
use codex_otel::THREAD_STARTED_METRIC;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Settings;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::LocalImagePreparation;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::DeprecationNoticeEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ModelRerouteEvent;
use codex_protocol::protocol::ModelRerouteReason;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::ModelVerificationEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RawResponseCompletedEvent;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TokenUsageRecord;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::HostSkillsService;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Queue and lifecycle endpoints for a running [`Session`].
///
/// Runtime state lives on `Session`; keeping these endpoints separate lets all
/// submission senders be dropped to terminate the session loop. The shared
/// completion future observes that shutdown.
#[derive(Clone)]
pub(crate) struct SessionIo {
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
    // Last known status of the agent.
    pub(crate) agent_status: watch::Receiver<AgentStatus>,
    // Shared future for the background submission loop completion so multiple
    // callers can wait for shutdown.
    pub(crate) session_loop_termination: SessionLoopTermination,
}

pub(crate) type SessionLoopTermination = Shared<BoxFuture<'static, ()>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitEnrichmentPolicy {
    Fresh,
    Skip,
}

pub(crate) struct SessionSpawnArgs {
    pub(crate) startup: Option<Arc<startup::SessionStartup>>,
    pub(crate) config: Config,
    pub(crate) allow_provider_model_fallback: bool,
    pub(crate) instructions: SessionInstructions,
    pub(crate) installation_id: String,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) git_root_discovery: Arc<GitRootDiscovery>,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) skills_service: Arc<HostSkillsService>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
    pub(crate) conversation_history: InitialHistory,
    pub(crate) history_initialization: HistoryInitialization,
    pub(crate) requested_history_mode: Option<ThreadHistoryMode>,
    pub(crate) session_source: SessionSource,
    pub(crate) forked_from_thread_id: Option<ThreadId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) originator: String,
    pub(crate) agent_control: AgentControlInit,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) metrics_service_name: Option<String>,
    pub(crate) inherited_environments: Option<TurnEnvironmentSnapshot>,
    pub(crate) user_shell_override: Option<shell::Shell>,
    pub(crate) parent_trace: Option<W3cTraceContext>,
    pub(crate) environment_selections: Vec<TurnEnvironmentSelection>,
    pub(crate) thread_extension_init: ExtensionDataInit,
    pub(crate) turn_extension_init: ExtensionDataInit,
    pub(crate) client_mcp_extensions: ClientMcpExtensions,
    pub(crate) reserved_thread_id: Option<ThreadId>,
    pub(crate) thread_store: Arc<ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) external_time_provider: Option<Arc<dyn TimeProvider>>,
    pub(crate) inherited_multi_agent_version: Option<MultiAgentVersion>,
    pub(crate) git_enrichment_policy: GitEnrichmentPolicy,
}

pub(crate) fn resolve_multi_agent_version(
    conversation_history: &InitialHistory,
    inherited_multi_agent_version: Option<MultiAgentVersion>,
) -> Option<MultiAgentVersion> {
    if inherited_multi_agent_version == Some(MultiAgentVersion::Disabled) {
        return Some(MultiAgentVersion::Disabled);
    }

    conversation_history
        .get_multi_agent_version()
        .or(inherited_multi_agent_version)
        .or(match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => None,
            // Threads created before runtime metadata existed keep the legacy V1 tool surface.
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => Some(MultiAgentVersion::V1),
        })
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 512;
const CYBER_VERIFY_URL: &str = "https://chatgpt.com/cyber";
const CYBER_SAFETY_URL: &str = "https://developers.openai.com/codex/concepts/cyber-safety";

impl Session {
    /// Spawn and initialize a new session.
    /// Hide the concrete startup future from callers while keeping initialization lazy.
    #[inline(never)]
    pub(crate) fn spawn(
        args: SessionSpawnArgs,
    ) -> BoxFuture<'static, CodexResult<(Arc<Self>, SessionIo)>> {
        let tree_shutdown = args.agent_control.runtime().shutdown.clone();
        Box::pin(async move {
            let parent_trace = match args.parent_trace {
                Some(trace) => {
                    if codex_otel::context_from_w3c_trace_context(&trace).is_some() {
                        Some(trace)
                    } else {
                        warn!("ignoring invalid thread spawn trace carrier");
                        None
                    }
                }
                None => None,
            };
            let thread_spawn_span = info_span!("thread_spawn", otel.name = "thread_spawn");
            if let Some(trace) = parent_trace.as_ref() {
                let _ = set_parent_from_w3c_trace_context(&thread_spawn_span, trace);
            }
            let spawn = Self::spawn_internal(SessionSpawnArgs {
                parent_trace,
                ..args
            })
            .instrument(thread_spawn_span);
            tokio::select! {
                biased;
                _ = tree_shutdown.cancelled() => Err(CodexErr::TurnAborted),
                result = spawn => result,
            }
        })
    }

    async fn spawn_internal(args: SessionSpawnArgs) -> CodexResult<(Arc<Self>, SessionIo)> {
        let SessionSpawnArgs {
            startup,
            config,
            allow_provider_model_fallback,
            instructions,
            installation_id,
            auth_manager,
            models_manager,
            git_root_discovery,
            environment_manager,
            skills_service,
            mcp_manager,
            extensions,
            conversation_history,
            history_initialization,
            requested_history_mode,
            session_source,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            agent_control,
            dynamic_tools,
            metrics_service_name,
            user_shell_override,
            inherited_environments,
            parent_trace: _,
            environment_selections,
            thread_extension_init,
            turn_extension_init,
            client_mcp_extensions,
            reserved_thread_id,
            thread_store,
            attestation_provider,
            external_time_provider,
            inherited_multi_agent_version,
            git_enrichment_policy,
        } = args;
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let isolation = thread_extension_init
            .get::<codex_extension_api::SessionIsolation>()
            .map(|policy| *policy)
            .unwrap_or_default();
        // Enforce snapshot-only instructions for both managed and inline isolated sessions.
        let instructions = if isolation == codex_extension_api::SessionIsolation::Isolated {
            SessionInstructions {
                user: instructions.user,
                thread: instructions.thread,
                project_snapshot: instructions.project_snapshot,
                ..Default::default()
            }
        } else {
            instructions
        };
        let mut config = Arc::new(config);
        let refresh_strategy = if session_source.is_non_root_agent() {
            codex_models_manager::manager::RefreshStrategy::Offline
        } else {
            codex_models_manager::manager::RefreshStrategy::OnlineIfUncached
        };
        if config.model.is_none()
            || !matches!(
                refresh_strategy,
                codex_models_manager::manager::RefreshStrategy::Offline
            )
        {
            let _ = models_manager
                .list_models(refresh_strategy, config.http_client_factory())
                .await;
        }
        let model = models_manager
            .get_default_model(
                &config.model,
                allow_provider_model_fallback,
                refresh_strategy,
                config.http_client_factory(),
            )
            .await;
        if model.trim().is_empty() {
            return Err(CodexErr::InvalidRequest(
                "No models are available. Set `model` explicitly or check your model catalog configuration.".to_string(),
            ));
        }
        if allow_provider_model_fallback
            && let Some(requested_model) = config.model.as_ref()
            && model != *requested_model
        {
            info!(
                model_provider = %config.model_provider_id,
                requested_model,
                fallback_model = %model,
                "replaced unavailable requested model with provider default"
            );
        }

        // Resolve base instructions for the session. Priority order:
        // 1. config.base_instructions override
        // 2. conversation history => session_meta.base_instructions
        // 3. rendered instructions_template for current model
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let auth = auth_manager.auth_cached();
        // Forked subagents keep their parent's activation with the copied history.
        // Fresh children restore configured preferences before applying startup defaults.
        let inherits_token_budget = matches!(&conversation_history, InitialHistory::Forked(_))
            && config.token_budget_startup_config.is_some();
        if !inherits_token_budget {
            Arc::make_mut(&mut config).prepare_token_budget_for_startup();
            // Resolve activation for this runtime, including when resuming saved history.
            token_budget::apply_experimental_context(
                Arc::make_mut(&mut config),
                auth.as_ref(),
                &model_info,
            )?;
            token_budget::apply_model_defaults(Arc::make_mut(&mut config), &model_info);
        }
        let configured_config = Arc::clone(&config);
        let multi_agent_version = config.multi_agent_version_override().or_else(|| {
            resolve_multi_agent_version(&conversation_history, inherited_multi_agent_version)
        });
        let history_mode =
            conversation_history.get_history_mode(requested_history_mode.unwrap_or_default());
        let base_instructions = config
            .base_instructions
            .clone()
            .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
            .unwrap_or_else(|| render_model_instructions(&model_info));

        // Dynamic tools are defined at thread start and persisted in rollout session metadata.
        let dynamic_tools = if dynamic_tools.is_empty() {
            conversation_history.get_dynamic_tools().unwrap_or_default()
        } else {
            dynamic_tools
        };
        // TODO (aibrahim): Consolidate config.model and config.model_reasoning_effort into config.collaboration_mode
        // to avoid extracting these fields separately and constructing CollaborationMode here.
        let persisted_collaboration_mode = match &conversation_history {
            InitialHistory::Resumed(resumed) => resumed
                .history
                .iter()
                .rev()
                .find_map(|item| match item {
                    RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))
                        if event.thread_id == Some(resumed.conversation_id) =>
                    {
                        Some(&event.thread_settings.collaboration_mode)
                    }
                    _ => None,
                })
                .or_else(|| {
                    resumed
                        .history
                        .iter()
                        .rev()
                        .find_map(|item| match item {
                            RolloutItem::TurnContext(context) => {
                                Some(context.collaboration_mode.as_ref())
                            }
                            _ => None,
                        })
                        .flatten()
                }),
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
        };
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: model.clone(),
                reasoning_effort: config.model_reasoning_effort.clone(),
                developer_instructions: None,
            },
        };
        let collaboration_mode = persisted_collaboration_mode.map_or(collaboration_mode, |saved| {
            saved.with_updates(
                Some(model.clone()),
                Some(config.model_reasoning_effort.clone()),
                /*developer_instructions*/ None,
            )
        });
        let initial_service_tier_warning = unsupported_service_tier_warning(
            config.service_tier.as_deref(),
            &config.features,
            &model_info,
        );
        let service_tier =
            get_service_tier(config.service_tier.clone(), &config.features, &model_info);
        let storage_originator = AuthStorageOriginator::from_client_name(&originator);
        let session_configuration = SessionConfiguration {
            turn_extension_init,
            provider: create_model_provider(
                config.model_provider.clone(),
                Some(Arc::clone(&auth_manager)),
            ),
            step_settings: Arc::new(StepSettings {
                collaboration_mode,
                reasoning_summary: config.model_reasoning_summary,
                service_tier,
                personality: config.personality,
            }),
            environments: environment_selections.clone(),
            model_info_overrides: config.to_models_manager_config().into(),
            developer_instructions: config.developer_instructions.clone(),
            base_instructions,
            allow_login_shell: config.allow_login_shell,
            shell_environment_policy: config.shell_environment_policy.clone(),
            legacy_fallback_cwd: config.cwd.clone(),
            runtime_workspace_roots: config.workspace_roots.clone(),
            codex_home: config.codex_home.clone(),
            original_config_do_not_use: Arc::clone(&config),
            metrics_service_name,
            app_server_client_name: None,
            app_server_client_version: None,
            session_source,
            history_mode,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            dynamic_tools,
            user_shell_override,
        };
        // Generate a unique ID for the lifetime of this session.
        let session_source_clone = session_configuration.session_source.clone();
        let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);

        let session = Box::pin(Session::new(
            startup.clone(),
            session_configuration,
            &environment_selections,
            config.clone(),
            instructions,
            installation_id,
            auth_manager.clone(),
            models_manager.clone(),
            git_root_discovery,
            model_info,
            tx_event.clone(),
            agent_status_tx.clone(),
            conversation_history,
            history_initialization,
            session_source_clone,
            skills_service,
            mcp_manager.clone(),
            extensions,
            thread_extension_init,
            client_mcp_extensions,
            agent_control,
            reserved_thread_id,
            environment_manager,
            inherited_environments,
            thread_store,
            attestation_provider,
            external_time_provider,
            multi_agent_version,
            git_enrichment_policy,
        ))
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            match e.downcast::<CodexErr>() {
                Ok(error) => error,
                Err(error) => CodexErr::Fatal(format!("Failed to initialize session: {error:#}")),
            }
        })?;
        if let Some(message) = initial_service_tier_warning {
            session
                .send_event_raw(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent { message }),
                })
                .await;
        }
        let thread_id = session.thread_id;

        // This task will run until Op::Shutdown is received.
        let tree_teardown = startup
            .as_ref()
            .and_then(|startup| startup.session_teardown(thread_id));
        let session_for_loop = Arc::clone(&session);
        let session_loop_handle = tokio::spawn(Box::pin(storage_originator.scope(async move {
            submission_loop(session_for_loop, configured_config, rx_sub)
                .instrument(info_span!("session_loop", thread_id = %thread_id))
                .await;
            if let Some(tree_teardown) = tree_teardown {
                tree_teardown.complete();
            }
        })));
        let io = SessionIo {
            tx_sub,
            rx_event,
            agent_status: agent_status_rx,
            session_loop_termination: session_loop_termination_from_handle(session_loop_handle),
        };

        if let Some(startup) = startup {
            let _ = startup.io.set(io.clone());
            startup.persistence.lock().await.commit();
        }
        Ok((session, io))
    }
}

impl SessionIo {
    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub(crate) async fn submit(
        &self,
        op: impl Into<WithTurnExtensionData<Op>>,
    ) -> CodexResult<String> {
        self.submit_with_trace(
            op, /*trace*/ None, /*parent_turn_id*/ None, /*root_turn_id*/ None,
            /*residency_guard*/ None,
        )
        .await
    }

    pub(crate) async fn submit_with_trace(
        &self,
        op: impl Into<WithTurnExtensionData<Op>>,
        trace: Option<W3cTraceContext>,
        parent_turn_id: Option<String>,
        root_turn_id: Option<String>,
        residency_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> CodexResult<String> {
        let id = new_submission_id();
        let WithTurnExtensionData {
            request: op,
            turn_extension_init,
        } = op.into();
        let sub = Submission {
            id: id.clone(),
            op,
            turn_extension_init,
            trace,
            parent_turn_id,
            root_turn_id,
            residency_guard,
        };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    /// Use sparingly: prefer `submit()` so submission IDs are generated consistently.
    pub(crate) async fn submit_with_id(&self, mut sub: Submission) -> CodexResult<()> {
        if sub.trace.is_none() {
            sub.trace = current_span_w3c_trace_context();
        }
        self.tx_sub
            .send(sub)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(())
    }

    /// Submits an ordered turn-input call and waits only for Core's routing decision.
    ///
    /// Once queued, dropping the waiter does not retract the call. If the
    /// session loop exits before replying, the caller gets `InternalAgentDied`.
    pub(crate) async fn submit_turn_input(
        &self,
        request: impl Into<WithTurnExtensionData<TurnInputRequest>>,
        mode: TurnInputMode,
    ) -> CodexResult<TurnInputSubmission> {
        let id = new_submission_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        let WithTurnExtensionData {
            mut request,
            turn_extension_init,
        } = request.into();
        let trace = request.trace.take();
        self.submit_with_id(Submission {
            id,
            op: Op::TurnInput {
                request: Box::new(request),
                mode,
                reply: reply_tx,
            },
            trace,
            turn_extension_init,
            parent_turn_id: None,
            root_turn_id: None,
            residency_guard: None,
        })
        .await?;
        reply_rx.await.unwrap_or(Err(CodexErr::InternalAgentDied))
    }

    pub(crate) async fn shutdown_and_wait(&self) -> CodexResult<()> {
        let session_loop_termination = self.session_loop_termination.clone();
        match self.submit(Op::Shutdown).await {
            Ok(_) => {}
            Err(err) if matches!(err.details(), CodexErrorDetails::InternalAgentDied) => {}
            Err(err) => return Err(err),
        }
        session_loop_termination.await;
        Ok(())
    }

    pub(crate) async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        self.agent_status.borrow().clone()
    }
}

/// Generate a core submission ID. App-server exposes submission IDs that
/// create turns as a public-facing turn ID.
///
/// Some use cases take advantage of the fact that these are UUID7 which
/// encodes a timestamp, so think carefully before changing this.
pub(crate) fn new_submission_id() -> String {
    Uuid::now_v7().to_string()
}

pub(crate) fn get_service_tier(
    configured_service_tier: Option<String>,
    features: &Features,
    model_info: &ModelInfo,
) -> Option<String> {
    let service_tier = configured_service_tier?;
    if service_tier == ServiceTier::Flex.request_value() {
        return Some(service_tier);
    }
    if features.service_tier_enabled(&service_tier)
        && (service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE
            || model_info.supports_service_tier(&service_tier))
    {
        return Some(service_tier);
    }
    None
}

fn unsupported_service_tier_warning(
    configured_service_tier: Option<&str>,
    features: &Features,
    model_info: &ModelInfo,
) -> Option<String> {
    let service_tier = configured_service_tier.filter(|service_tier| {
        features.service_tier_enabled(service_tier)
            && *service_tier != SERVICE_TIER_DEFAULT_REQUEST_VALUE
            && !model_info.supports_service_tier(service_tier)
    })?;
    Some(format!(
        "Configured service tier `{service_tier}` is not advertised as supported for model `{}` and will be omitted from requests.",
        model_info.slug
    ))
}

pub(crate) fn session_loop_termination_from_handle(
    handle: JoinHandle<()>,
) -> SessionLoopTermination {
    async move {
        let _ = handle.await;
    }
    .boxed()
    .shared()
}

impl Session {
    /// Builds the `x-codex-beta-features` header value for this session.
    ///
    /// `ModelClient` is session-scoped and intentionally does not depend on the full `Config`, so
    /// we precompute the comma-separated list of enabled experimental feature keys at session
    /// creation time and thread it into the client. Remote compaction stays advertised unconditionally.
    fn build_model_client_beta_features_header(config: &Config) -> Option<String> {
        Some(
            FEATURES
                .iter()
                .filter(|spec| {
                    spec.id == Feature::RemoteCompactionV2
                        || (spec.stage.experimental_menu_description().is_some()
                            && config.features.enabled(spec.id))
                })
                .map(|spec| spec.key)
                .collect::<Vec<_>>()
                .join(","),
        )
    }

    pub(crate) fn subscribe_elicitation_pause_state(&self) -> watch::Receiver<bool> {
        self.services.elicitations.subscribe()
    }

    pub(crate) fn mark_interrupted(&self) {
        self.agent_status.send_replace(AgentStatus::Interrupted);
    }

    pub(crate) fn is_interrupted(&self) -> bool {
        matches!(*self.agent_status.borrow(), AgentStatus::Interrupted)
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    pub(crate) fn live_thread(&self) -> Option<&LiveThread> {
        self.services.live_thread.as_ref()
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    pub(crate) async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage(state.server_reasoning_included())
    }

    pub(crate) async fn auto_compact_window_snapshot(&self) -> AutoCompactWindowSnapshot {
        let state = self.state.lock().await;
        state.auto_compact_window_snapshot()
    }

    pub(crate) async fn total_token_usage(&self) -> Option<TokenUsage> {
        let state = self.state.lock().await;
        state.token_info().map(|info| info.total_token_usage)
    }

    /// Returns the complete token usage snapshot currently cached for this session.
    ///
    /// Resume and fork reconstruction seed this state from the last persisted rollout
    /// `TokenCount` event. Callers that need to replay restored usage to a client
    /// should use this accessor instead of `total_token_usage`, because the app-server
    /// notification includes both total and last-turn usage.
    pub(crate) async fn token_usage_info(&self) -> Option<TokenUsageInfo> {
        let state = self.state.lock().await;
        state.token_info()
    }

    pub(crate) async fn get_estimated_token_count(
        &self,
        turn_context: &TurnContext,
    ) -> Option<i64> {
        let history = self.clone_history().await;
        history.estimate_token_count(turn_context)
    }

    pub(crate) async fn get_base_instructions(&self) -> BaseInstructions {
        let state = self.state.lock().await;
        BaseInstructions {
            text: state.session_configuration.base_instructions.clone(),
            provenance: state.base_instructions_provenance.clone(),
        }
    }

    /// Render the request copy without changing instructions persisted or inherited by forks.
    pub(crate) async fn get_prompt_base_instructions(&self) -> BaseInstructions {
        let config = self.get_config().await;
        let instructions = self.get_base_instructions().await;
        if !config.update_plan_enabled
            && config.model_catalog.is_none()
            && matches!(
                instructions.provenance,
                Some(BaseInstructionsProvenance::Model { .. })
            )
        {
            BaseInstructions {
                text: codex_prompts::without_update_plan_instructions(&instructions.text),
                ..instructions
            }
        } else {
            instructions
        }
    }

    async fn record_initial_history(&self, conversation_history: InitialHistory) {
        let is_paginated_subagent = {
            let state = self.state.lock().await;
            let session_configuration = &state.session_configuration;
            matches!(
                session_configuration.history_mode,
                ThreadHistoryMode::Paginated
            ) && matches!(
                session_configuration.thread_source.as_ref(),
                Some(ThreadSource::Subagent)
            )
        };
        match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => {
                // Defer initial context insertion until the first real turn starts so
                // turn/start overrides can be merged before we write model-visible context.
                self.set_previous_turn_settings(/*previous_turn_settings*/ None)
                    .await;
            }
            InitialHistory::Resumed(resumed_history) => {
                let turn_context = self.new_default_turn().await;
                let rollout_items = resumed_history.history;
                if matches!(
                    rollout_items.iter().rev().find_map(|item| match item {
                        RolloutItem::EventMsg(event) => agent_status_from_event(event),
                        _ => None,
                    }),
                    Some(AgentStatus::Interrupted)
                ) {
                    self.agent_status.send_replace(AgentStatus::Interrupted);
                }
                let previous_turn_settings = self
                    .apply_rollout_reconstruction(&turn_context, &rollout_items)
                    .await;

                // If resuming, warn when the last recorded model differs from the current one.
                let curr: &str = turn_context.model_info().slug.as_str();
                if let Some(prev) = previous_turn_settings
                    .as_ref()
                    .map(|settings| settings.model.as_str())
                    .filter(|model| *model != curr)
                {
                    warn!("resuming session with different model: previous={prev}, current={curr}");
                    self.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                            ),
                        }),
                    )
                    .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }
                self.state.lock().await.latest_token_usage_record =
                    Self::last_token_usage_record_from_rollout(&rollout_items);

                // Checkpoint effective settings even when no turn follows the resume.
                self.persist_rollout_items(&[RolloutItem::EventMsg(
                    thread_settings::applied_event(self).await,
                )])
                .await;
                // Initial context is seeded on the first turn so turn/start overrides apply.
            }
            InitialHistory::Forked(mut rollout_items) => {
                let turn_context = self.new_default_turn().await;
                Self::assign_missing_rollout_response_item_ids(&mut rollout_items);
                self.apply_rollout_reconstruction(&turn_context, &rollout_items)
                    .await;

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }
                self.state.lock().await.latest_token_usage_record =
                    Self::last_token_usage_record_from_rollout(&rollout_items);

                let thread_settings_applied =
                    RolloutItem::EventMsg(thread_settings::applied_event(self).await);
                if is_paginated_subagent {
                    // Paginated subagents already record inherited context when their live
                    // thread is created.
                    rollout_items.clear();
                }
                rollout_items.push(thread_settings_applied);
                self.persist_rollout_items(&rollout_items).await;
            }
        }
    }

    #[instrument(
        level = "trace",
        skip_all,
        fields(
            thread_id = %self.thread_id(),
            rollout_item_count = rollout_items.len()
        )
    )]
    async fn apply_rollout_reconstruction(
        &self,
        turn_context: &Arc<TurnContext>,
        rollout_items: &[RolloutItem],
    ) -> Option<PreviousTurnSettings> {
        let rollout_reconstruction::RolloutReconstruction {
            mut history,
            retained_context,
            last_started_turn_id,
            previous_turn_settings,
            reference_context_item,
            turn_attribution,
            world_state_baseline,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = self
            .reconstruct_history_from_rollout(turn_context, rollout_items)
            .await;
        // Keep the recorded rollout unchanged. Prepare its reconstructed history before
        // installing it, so legacy media is processed once for this resume or fork and
        // will be processed again if the rollout is reconstructed in a future session.
        // Replay disables image-resize notices, so media preparation remains one-to-one. Keep
        // the prior batch behavior and carry history-only metadata in a positional sidecar.
        let (mut prepared_history, metadata): (Vec<_>, Vec<_>) = history
            .into_iter()
            .map(|envelope| (envelope.item, envelope.metadata))
            .unzip();
        prepare_image_response_items(
            &mut prepared_history,
            ImagePreparationMode::DetailBased,
            ImageResizeNoticeMode::Disabled,
        );
        prepare_audio_response_items(&mut prepared_history);
        assert_eq!(
            prepared_history.len(),
            metadata.len(),
            "replay media preparation must remain one-to-one when resize notices are disabled"
        );
        history = prepared_history
            .into_iter()
            .zip(metadata)
            .map(|(item, metadata)| ResponseItemEnvelope { item, metadata })
            .collect();
        {
            let mut state = self.state.lock().await;
            state.replace_annotated_history(
                history,
                reference_context_item,
                HistoryReplacement::Reset,
            );
            state
                .history
                .restore_retained_context(Some(&retained_context));
            state.last_started_turn_id = last_started_turn_id;
            state.turn_attribution = turn_attribution;
            if let Some(world_state) = world_state_baseline {
                state.history.set_world_state_baseline(world_state);
            }
            let fallback_ids = state.auto_compact_window_ids();
            let window_id = window_id.unwrap_or(fallback_ids.window_id);
            state.restore_auto_compact_window(
                window_number,
                AutoCompactWindowIds {
                    first_window_id: first_window_id.unwrap_or(window_id),
                    previous_window_id,
                    window_id,
                },
            );
            state.set_previous_turn_settings(previous_turn_settings.clone());
        }
        let prefix_tokens = if matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            let history = self.clone_history().await;
            let base_instructions = self.get_base_instructions().await;
            history.estimate_token_count_with_base_instructions(&base_instructions)
        } else {
            None
        };
        if let Some(prefix_tokens) = prefix_tokens {
            self.set_auto_compact_window_estimated_prefill_for_scope(turn_context, prefix_tokens)
                .await;
        }
        previous_turn_settings
    }

    async fn set_auto_compact_window_estimated_prefill_for_scope(
        &self,
        turn_context: &TurnContext,
        tokens: i64,
    ) {
        if !matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            return;
        }

        let mut state = self.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(tokens);
    }

    fn last_token_info_from_rollout(rollout_items: &[RolloutItem]) -> Option<TokenUsageInfo> {
        rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(ev)) => ev.info.clone(),
            _ => None,
        })
    }

    fn last_token_usage_record_from_rollout(
        rollout_items: &[RolloutItem],
    ) -> Option<TokenUsageRecord> {
        for item in rollout_items.iter().rev() {
            match item {
                RolloutItem::TokenUsageRecord(record) => return Some(record.clone()),
                RolloutItem::Compacted(compacted) => {
                    return compacted.latest_token_usage_record.clone();
                }
                _ => {}
            }
        }
        None
    }

    async fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        let state = self.state.lock().await;
        state.previous_turn_settings()
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn set_previous_turn_settings(
        &self,
        previous_turn_settings: Option<PreviousTurnSettings>,
    ) {
        let mut state = self.state.lock().await;
        state.set_previous_turn_settings(previous_turn_settings);
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> CodexResult<SessionSettingsCommit> {
        let Some(commit) = self.update_settings_if(updates, |_, _| true).await? else {
            unreachable!("unconditional settings updates must commit");
        };
        Ok(commit)
    }

    /// Evaluates the caller's synchronous predicate against the current configuration
    /// and validated candidate under the same state lock used for publication.
    /// This prevents a stale admission decision. Rejection has no settings or runtime
    /// effects.
    ///
    /// The predicate must be fast and side-effect-free. It must not block, acquire
    /// other locks, or call back into `Session`.
    async fn update_settings_if(
        &self,
        updates: SessionSettingsUpdate,
        should_commit: impl FnOnce(&SessionConfiguration, &SessionConfiguration) -> bool + Send,
    ) -> CodexResult<Option<SessionSettingsCommit>> {
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let (commit, previous_config, new_config, mcp_inputs_changed) = {
            let mut state = self.state.lock().await;
            let updated = match self.apply_session_settings(&state.session_configuration, &updates)
            {
                Ok(updated) => updated,
                Err(err) => {
                    warn!("rejected session settings update: {err}");
                    return Err(err);
                }
            };

            if !should_commit(&state.session_configuration, &updated) {
                return Ok(None);
            }

            let previous_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            let root_service_tier_changed = updated.parent_thread_id.is_none()
                && state.session_configuration.step_settings.service_tier
                    != updated.step_settings.service_tier;
            let mcp_inputs_changed = self.mcp_inputs_differ(&state.session_configuration, &updated);
            if mcp_inputs_changed {
                self.mark_mcp_runtime_dirty();
            }
            state.session_configuration = updated;
            if root_service_tier_changed {
                self.services.agent_control.propagate_config_update(
                    AgentConfigUpdate::ServiceTier(
                        state
                            .session_configuration
                            .step_settings
                            .service_tier
                            .clone(),
                    ),
                );
            }
            let new_config = notify_config_contributors
                .then(|| self.build_effective_session_config(&state.session_configuration));
            let commit = SessionSettingsCommit {
                configuration: state.session_configuration.clone(),
                snapshot: state.session_configuration.thread_settings_snapshot(),
            };
            (commit, previous_config, new_config, mcp_inputs_changed)
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        if mcp_inputs_changed {
            self.schedule_mcp_prewarm();
        }
        Ok(Some(commit))
    }

    pub(crate) async fn preview_settings(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> CodexResult<ThreadConfigSnapshot> {
        let state = self.state.lock().await;
        let configuration = self.apply_session_settings(&state.session_configuration, updates)?;
        Ok(configuration.thread_config_snapshot(configuration.environments.clone()))
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        let state = self.state.lock().await;
        state
            .session_configuration
            .thread_config_snapshot(state.session_configuration.environments.clone())
    }

    pub(crate) async fn configured_environment_selections(&self) -> Vec<TurnEnvironmentSelection> {
        let state = self.state.lock().await;
        state.session_configuration.environments.clone()
    }

    pub(crate) async fn thread_settings_snapshot(&self) -> ThreadSettingsSnapshot {
        let state = self.state.lock().await;
        state.session_configuration.thread_settings_snapshot()
    }

    pub(crate) async fn restorable_thread_settings(&self) -> CodexThreadSettingsOverrides {
        let state = self.state.lock().await;
        state
            .session_configuration
            .restorable_thread_settings(state.session_configuration.environments.clone())
    }

    pub(crate) async fn set_app_server_client_info(
        &self,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        mcp_elicitations_auto_deny: bool,
    ) -> CodexResult<()> {
        self.update_settings(SessionSettingsUpdate {
            app_server_client_name,
            app_server_client_version,
            ..Default::default()
        })
        .await?;
        self.services
            .mcp_runtime
            .set_elicitations_auto_deny(mcp_elicitations_auto_deny);
        Ok(())
    }

    pub(crate) async fn instruction_sources(&self) -> Vec<PathUri> {
        self.services
            .agents_md_manager
            .get_loaded()
            .await
            .as_ref()
            .map_or_else(Vec::new, |instructions| instructions.sources().collect())
    }

    pub(crate) async fn take_session_startup_prewarm(&self) -> Option<SessionStartupPrewarmHandle> {
        let mut state = self.state.lock().await;
        state.take_session_startup_prewarm()
    }

    pub(crate) async fn get_config(&self) -> std::sync::Arc<Config> {
        let state = self.state.lock().await;
        state
            .session_configuration
            .original_config_do_not_use
            .clone()
    }

    pub(crate) async fn inherited_instructions(&self) -> SessionInstructions {
        self.services
            .agents_md_manager
            .inherited_instructions()
            .await
    }

    pub(crate) async fn emit_instruction_warnings(&self, warnings: Vec<String>) {
        for message in warnings {
            self.send_event_raw(Event {
                id: INITIAL_SUBMIT_ID.to_owned(),
                msg: EventMsg::Warning(WarningEvent { message }),
            })
            .await;
        }
    }

    pub(crate) async fn provider(&self) -> ModelProviderInfo {
        let state = self.state.lock().await;
        state.session_configuration.provider.info().clone()
    }

    pub(crate) async fn refresh_hooks(&self, mut config: Arc<Config>) {
        loop {
            let environments = self.services.turn_environments.snapshot();
            let hooks_config = build_hooks_config(config.as_ref(), environments.primary());

            let state = self.state.lock().await;
            if Arc::ptr_eq(
                &state.session_configuration.original_config_do_not_use,
                &config,
            ) {
                let hooks = self.hooks().reconfigured(hooks_config);
                self.services.hooks.store(Arc::new(hooks));
                return;
            }
            // Unrelated updates can replace the owner too. Rebuild from current inputs.
            config = Arc::clone(&state.session_configuration.original_config_do_not_use);
        }
    }

    fn emit_config_changed_contributors(
        &self,
        previous_config: Option<&Config>,
        new_config: Option<&Config>,
    ) {
        let (Some(previous_config), Some(new_config)) = (previous_config, new_config) else {
            return;
        };
        if previous_config == new_config {
            return;
        }
        for contributor in self.services.extensions.config_contributors() {
            contributor.on_config_changed(
                &self.services.session_extension_data,
                &self.services.thread_extension_data,
                previous_config,
                new_config,
            );
        }
    }

    pub(crate) async fn reload_user_config_layer(&self) {
        // Refresh layer-backed runtime state for an existing session, including
        // skill, hook, and MCP feature state. Other feature gates and legacy notify settings
        // remain session-static.
        //
        // Prefer `refresh_runtime_config()` when the host can already provide a materialized
        // config snapshot. This file-based path exists for legacy local reload flows.
        // Retry from the current owner if another publication wins while files are read.
        loop {
            let (expected_config, config_toml_paths) = {
                let state = self.state.lock().await;
                let config = Arc::clone(&state.session_configuration.original_config_do_not_use);
                let user_config_paths = config
                    .config_layer_stack
                    .all_layers_low_to_high()
                    .filter_map(|layer| match &layer.name {
                        ConfigLayerSource::User { file, .. } => Some(file.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let paths = if user_config_paths.is_empty() {
                    vec![
                        state
                            .session_configuration
                            .codex_home
                            .join(CONFIG_TOML_FILE),
                    ]
                } else {
                    user_config_paths
                };
                (config, paths)
            };

            let mut reloaded_user_configs = Vec::with_capacity(config_toml_paths.len());
            for config_toml_path in config_toml_paths {
                let user_config = match std::fs::read_to_string(&config_toml_path) {
                    Ok(contents) => match toml::from_str::<toml::Value>(&contents) {
                        Ok(config) => config,
                        Err(err) => {
                            warn!("failed to parse user config while reloading layer: {err}");
                            return;
                        }
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        toml::Value::Table(Default::default())
                    }
                    Err(err) => {
                        warn!("failed to read user config while reloading layer: {err}");
                        return;
                    }
                };
                let Some(config_dir) = config_toml_path.parent() else {
                    warn!("user config path has no parent directory");
                    return;
                };
                let user_config = match codex_config::loader::resolve_relative_paths_in_config_toml(
                    user_config,
                    &config_dir,
                ) {
                    Ok(config) => config,
                    Err(err) => {
                        warn!("failed to resolve paths while reloading user config: {err}");
                        return;
                    }
                };
                reloaded_user_configs.push((config_toml_path, user_config));
            }

            let mut next_config = expected_config.as_ref().clone();
            for (config_toml_path, user_config) in reloaded_user_configs {
                let config_layer_stack = match next_config
                    .config_layer_stack
                    .with_user_config(&config_toml_path, user_config)
                {
                    Ok(config_layer_stack) => config_layer_stack,
                    Err(err) => {
                        warn!("failed to validate user config while reloading layer: {err}");
                        return;
                    }
                };
                next_config.config_layer_stack = config_layer_stack;
            }
            match self
                .refresh_config(
                    expected_config,
                    next_config,
                    crate::config::RuntimeConfigRefresh::UserFiles,
                )
                .await
            {
                crate::ConfigRefreshOutcome::Stale => continue,
                crate::ConfigRefreshOutcome::Published | crate::ConfigRefreshOutcome::Rejected => {
                    return;
                }
            }
        }
    }

    pub(crate) async fn emit_turn_started(&self, turn_context: &TurnContext, task_kind: TaskKind) {
        let event = TurnStartedEvent {
            turn_attribution: (task_kind == TaskKind::Regular).then(|| turn_context.attribution()),
            turn_id: turn_context.sub_id.clone(),
            root_turn_id: Some(turn_context.root_turn_id()),
            trace_id: turn_context.trace_id.clone(),
            started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
            model_context_window: turn_context.model_context_window(),
            collaboration_mode_kind: turn_context.mode(),
        };
        self.send_event(turn_context, EventMsg::TurnStarted(event))
            .await;
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        let legacy_source = msg.clone();
        if let EventMsg::Error(error) = &legacy_source
            && error
                .codex_error_info
                .as_ref()
                .is_some_and(CodexErrorInfo::affects_turn_status)
        {
            turn_context
                .terminal_error
                .lock()
                .await
                .replace(error.clone());
        }
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        self.send_event_raw(event).await;
        self.maybe_notify_parent_of_terminal_turn(turn_context, &legacy_source)
            .await;

        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        for legacy in legacy_source.as_legacy_events(show_raw_agent_reasoning) {
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
    }

    /// Forwards terminal turn events from spawned MultiAgentV2 children to their direct parent.
    async fn maybe_notify_parent_of_terminal_turn(
        &self,
        turn_context: &TurnContext,
        msg: &EventMsg,
    ) {
        if turn_context.multi_agent_version != MultiAgentVersion::V2 {
            return;
        }

        if !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
            return;
        }

        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_path: Some(_),
            ..
        }) = &turn_context.session_source
        else {
            return;
        };

        let status = match turn_context.terminal_error.lock().await.take() {
            Some(error) => {
                let status = AgentStatus::Errored(error.message);
                self.agent_status.send_replace(status.clone());
                status
            }
            None => {
                let Some(status) = agent_status_from_event(msg) else {
                    return;
                };
                status
            }
        };
        if !is_final(&status) {
            return;
        }

        self.services
            .agent_control
            .turn_finished(AgentTurnOutcome {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                source: turn_context.session_source.clone(),
                parent_turn_id: turn_context.turn_metadata_state.parent_turn_id(),
                initiating_agent_path: turn_context
                    .turn_metadata_state
                    .initiating_agent_path()
                    .cloned(),
                status,
            })
            .await;
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        // Record the event in thread history; the store applies its persistence policy.
        let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
        self.persist_rollout_items(&rollout_items).await;
        self.deliver_event_raw(event).await;
    }

    async fn deliver_event_raw(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.agent_status.send_replace(status);
        }
        if let Err(e) = self.tx_event.send(event).await {
            debug!("dropping event because channel is closed: {e}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        let started_at_ms = turn_context
            .turn_timing_state
            .record_item_started(item.id(), now_unix_timestamp_ms())
            .await;
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
                started_at_ms,
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        record_turn_ttfm_metric(turn_context, &item).await;
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_item_completed(
                    &self.services.thread_extension_data,
                    turn_context.extension_data.as_ref(),
                    &item,
                )
                .await;
        }
        let completed_at_ms = now_unix_timestamp_ms();
        let item_id = item.id();
        let started_at_ms = turn_context
            .turn_timing_state
            .take_item_started(&item_id)
            .await
            .unwrap_or_else(|| {
                warn!(
                    thread_id = %self.thread_id,
                    turn_id = %turn_context.sub_id,
                    item_id = %item_id,
                    "item completed without a recorded start timestamp"
                );
                completed_at_ms
            });
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item,
                started_at_ms: Some(started_at_ms),
                completed_at_ms,
            }),
        )
        .await;
    }

    async fn active_turn_context_and_cancellation_token(
        &self,
    ) -> Option<(Arc<TurnContext>, CancellationToken)> {
        let active = self.active_turn.lock().await;
        let task = active.as_ref()?.task.as_ref()?;
        Some((
            Arc::clone(&task.turn_context),
            task.cancellation_token.child_token(),
        ))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn request_user_input(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let _elicitation = self.services.elicitations.register();
        let sub_id = turn_context.sub_id.clone();
        let (tx_response, rx_response) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_user_input(sub_id, tx_response)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending user input for sub_id: {event_id}");
        }

        let event = EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            questions: args.questions,
            is_blocking: args.is_blocking,
            auto_resolution_ms: args.auto_resolution_ms,
        });
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        rx_response.await.ok()
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_user_input_response(
        &self,
        sub_id: &str,
        response: RequestUserInputResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => at.turn_state.lock().await.remove_pending_user_input(sub_id),
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending user input found for sub_id: {sub_id}");
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_dynamic_tool_response(&self, call_id: &str, response: DynamicToolResponse) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_dynamic_tool(call_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending dynamic tool call found for call_id: {call_id}");
            }
        }
    }

    pub(crate) fn response_item_create_time() -> serde_json::Number {
        let create_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        serde_json::Number::from_f64(create_time.as_secs_f64())
            .unwrap_or_else(|| serde_json::Number::from(create_time.as_secs()))
    }

    /// Adds the turn ID and creation time to newly created conversation items.
    pub(crate) fn stamp_response_item_for_history(item: &mut ResponseItem, turn_id: &str) {
        item.set_turn_id_if_missing(turn_id);
        item.set_create_time_if_missing(Self::response_item_create_time());
    }

    /// Prepares media using the originating model and preserves existing item identity.
    pub(crate) async fn prepare_conversation_items_for_history<'a>(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        items: &'a [ResponseItem],
    ) -> Cow<'a, [ResponseItem]> {
        let mut items = items.to_vec();
        let image_preparation_mode =
            if unified_image_budget_enabled(&turn_context.config.features, model_info) {
                ImagePreparationMode::UnifiedBudget
            } else {
                ImagePreparationMode::DetailBased
            };
        let image_resize_notice_mode = if turn_context
            .config
            .features
            .enabled(Feature::ImageResizeNotice)
        {
            ImageResizeNoticeMode::Enabled
        } else {
            ImageResizeNoticeMode::Disabled
        };
        prepare_image_response_items(&mut items, image_preparation_mode, image_resize_notice_mode);
        prepare_audio_response_items(&mut items);
        // Most response items get their passthrough turn ID at the durable history boundary.
        for item in &mut items {
            Self::stamp_response_item_for_history(item, &turn_context.sub_id);
        }
        Self::assign_missing_response_item_ids(Cow::Owned(items))
    }

    fn assign_missing_response_item_ids(items: Cow<'_, [ResponseItem]>) -> Cow<'_, [ResponseItem]> {
        if items
            .iter()
            .all(|item| item.id().is_some_and(|id| !id.is_empty()))
        {
            return items;
        }
        let mut items = items;
        for item in items.to_mut() {
            Self::assign_missing_response_item_id(item);
        }
        items
    }

    pub(crate) fn assign_missing_response_item_id(item: &mut ResponseItem) {
        if item.id().is_some_and(|id| !id.is_empty()) {
            return;
        }
        let Some(prefix) = item.id_prefix() else {
            return;
        };
        item.set_id(Some(ResponseItemId::new(prefix)));
    }

    fn assign_missing_rollout_response_item_ids(items: &mut [RolloutItem]) {
        for item in items {
            if let RolloutItem::ResponseItem(response_item) = item {
                Self::assign_missing_response_item_id(&mut response_item.item);
            }
        }
    }

    fn response_item_from_user_input_with_image_positions(
        &self,
        input: Vec<UserInput>,
        user_image_content_indices: &mut HashMap<usize, usize>,
    ) -> ResponseItem {
        let mut item = ResponseItem::from(ResponseInputItem::from_user_input(
            input,
            LocalImagePreparation::Defer,
            user_image_content_indices,
        ));
        if let ResponseItem::Message {
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut item
        {
            let content_item_kinds = content
                .iter()
                .map(|content| {
                    ContentItemKind(
                        match content {
                            ContentItem::InputText { .. } | ContentItem::OutputText { .. } => {
                                "user.text"
                            }
                            ContentItem::InputImage { .. } => "user.image",
                            ContentItem::InputAudio { .. } => "user.audio",
                        }
                        .to_string(),
                    )
                })
                .collect();
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(content_item_kinds),
                    ..Default::default()
                });
        }
        item
    }

    /// Appends to history, persists the prepared items, then notifies raw-item observers.
    /// Execution callers supply their captured model; standalone callers need no tool runtime.
    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        items: &[ResponseItem],
    ) {
        let items = self
            .prepare_conversation_items_for_history(turn_context, model_info, items)
            .await;
        let items = items
            .into_owned()
            .into_iter()
            .map(ResponseItemEnvelope::new)
            .collect();
        self.record_prepared_conversation_items(turn_context, model_info, items)
            .await;
    }

    async fn record_prepared_conversation_items(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        mut items: Vec<ResponseItemEnvelope>,
    ) {
        // Save the originating history budget for replay.
        // Preserve any existing tool-specific override.
        let policy: codex_utils_output_truncation::TruncationPolicy =
            model_info.truncation_policy.into();
        for envelope in &mut items {
            if matches!(
                envelope.item,
                ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
            ) {
                envelope
                    .metadata
                    .get_or_insert_default()
                    .history_truncation_token_limit
                    .get_or_insert_with(|| with_serialization_allowance(policy).token_budget());
            }
        }
        // Last-N-turn forks retain a suffix starting at a user turn boundary. Repeat the
        // cumulative checkpoint there so the suffix remains self-contained even when the fork
        // happens mid-turn; dirty checkpoints preserve new sources between boundaries.
        let force_mcp_checkpoint = items
            .iter()
            .any(|envelope| crate::context_manager::is_user_turn_boundary(&envelope.item));
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(force_mcp_checkpoint)
            .and_then(|(attribution, revision)| {
                let first_persisted = items
                    .iter()
                    .position(|envelope| should_persist_response_item(&envelope.item))?;
                for (index, envelope) in items.iter_mut().enumerate() {
                    // Rollout batches may be partially written. Checkpoint the first persisted
                    // item, and repeat the checkpoint at turn boundaries retained by forks.
                    if index == first_persisted
                        || crate::context_manager::is_user_turn_boundary(&envelope.item)
                    {
                        envelope.metadata.get_or_insert_default().mcp_attribution =
                            Some(attribution.clone());
                    }
                }
                Some(revision)
            });
        let response_items = items
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect::<Vec<_>>();
        {
            let mut state = self.state.lock().await;
            state
                .current_time_reminder
                .note_recorded_items(&response_items);
            let pending_orders = turn_context
                .extension_data
                .get::<retained_context::PendingAssistantMessageOrders>();
            for envelope in &mut items {
                if envelope
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.compaction_output)
                {
                    continue;
                }
                if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "assistant")
                    || matches!(&envelope.item, ResponseItem::FunctionCall { .. })
                    || crate::context::is_user_instruction_message(&envelope.item)
                {
                    let message_order = pending_orders.as_ref().and_then(|orders| {
                        orders
                            .0
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(envelope.item.id()?.as_str())
                    });
                    // Preserve input acceptance and source-message start order.
                    // Synthetic messages still receive their order here.
                    envelope
                        .metadata
                        .get_or_insert_default()
                        .user_input_order
                        .get_or_insert_with(|| {
                            message_order.unwrap_or_else(|| state.history.reserve_input_order())
                        });
                }
            }
            state
                .history
                .record_annotated_items(&mut items, model_info.truncation_policy.into());
        }
        let rollout_items: Vec<RolloutItem> =
            items.into_iter().map(RolloutItem::ResponseItem).collect();
        if self.persist_rollout_items(&rollout_items).await
            && let Some(revision) = mcp_revision
        {
            self.services
                .executed_tool_calls
                .mark_mcp_attribution_persisted(revision);
        }
        self.send_raw_response_items(turn_context, &response_items)
            .await;
    }

    pub(crate) async fn record_step_world_state_if_changed(
        &self,
        step_context: &step_context::StepContext,
    ) -> CodexResult<()> {
        let turn_context = step_context.turn.as_ref();
        // Render model-visible state from the same step used to build and run tools.
        let world_state = self
            .build_world_state_for_step(step_context, /*new_window*/ false)
            .await?;
        let (world_state_snapshot, fragments, world_state_item) = self
            .state
            .lock()
            .await
            .history
            .render_step_world_state(&world_state);
        let items = crate::context_manager::updates::merge_world_state_updates(fragments);
        if !items.is_empty() {
            self.record_conversation_items(turn_context, &step_context.settings.model_info, &items)
                .await;
        }

        self.state
            .lock()
            .await
            .history
            .set_world_state_baseline(world_state_snapshot);
        // Record the patch after the context it describes is present in model history.
        if let Some(world_state_item) = world_state_item {
            self.persist_rollout_items(&[RolloutItem::WorldState(world_state_item)])
                .await;
        }
        Ok(())
    }

    /// Retains the step captured for execution.
    pub(crate) async fn set_last_known_step_context(&self, step_context: &Arc<StepContext>) {
        let turn_state = {
            let active_turn = self.active_turn.lock().await;
            active_turn.as_ref().and_then(|active_turn| {
                active_turn
                    .task
                    .as_ref()
                    .filter(|task| task.turn_context.sub_id == step_context.turn.sub_id)
                    .map(|_| Arc::clone(&active_turn.turn_state))
            })
        };
        if let Some(turn_state) = turn_state {
            turn_state.lock().await.last_known_step_context = Some(Arc::clone(step_context));
        }
    }

    /// Captures one request-scoped view of dynamic state and retains it for the active turn.
    ///
    /// This may refresh filesystem-derived state. Normal turns should call it only from
    /// `run_turn` and pass the result down; standalone request or history boundaries may capture
    /// their own step. Use speculative capture for a step that may not execute.
    pub(crate) async fn capture_step_context(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<Arc<StepContext>> {
        self.capture_step_context_with_required_mcp_servers(
            turn_context,
            cancellation_token,
            /*required_servers*/ &[],
        )
        .await
    }

    /// Captures and retains a step with the required MCP startup dependencies.
    pub(crate) async fn capture_step_context_with_required_mcp_servers(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
        required_servers: &[String],
    ) -> CodexResult<Arc<StepContext>> {
        let step_context = self
            .capture_step_context_inner(turn_context, cancellation_token, required_servers)
            .await?;
        self.set_last_known_step_context(&step_context).await;
        Ok(step_context)
    }

    /// Prepares a candidate step without replacing the active turn's retained context.
    /// The caller must retain it explicitly if it is selected for execution.
    async fn capture_speculative_step_context(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<Arc<StepContext>> {
        self.capture_step_context_inner(
            turn_context,
            cancellation_token,
            /*required_servers*/ &[],
        )
        .await
    }

    #[tracing::instrument(name = "step_context.capture", level = "info", skip_all)]
    async fn capture_step_context_inner(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: &CancellationToken,
        required_servers: &[String],
    ) -> CodexResult<Arc<StepContext>> {
        // Read the step's model and record its environments together so an update cannot split them.
        let (mut settings, environments) = {
            let _active = self
                .active_turn
                .lock()
                .or_cancel(cancellation_token)
                .await?;
            (
                turn_context.next_step_settings.load_full(),
                self.services.turn_environments.snapshot(),
            )
        };
        if matches!(
            turn_context.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        ) {
            let root_service_tier = self.services.agent_control.service_tier();
            if settings.selected().service_tier != root_service_tier {
                let mut selected = settings.selected().clone();
                selected.service_tier = root_service_tier;
                settings = Arc::new(ResolvedStepSettings::new(
                    Arc::new(selected),
                    Arc::clone(&settings.model_info),
                    &self.features,
                ));
            }
        }
        let token_budget = token_budget::resolve_token_budget(
            turn_context.configured_token_budget.as_ref(),
            turn_context.use_model_token_budget_defaults,
            settings.model_info.as_ref(),
        );
        let session_telemetry = settings.telemetry(&turn_context.session_telemetry);
        // Keep both preparation futures off caller stacks while they are live together.
        let load_agents_md = Box::pin(async {
            let (loaded_agents_md, warnings) = self
                .services
                .agents_md_manager
                .refresh(&turn_context.config, &environments)
                .or_cancel(cancellation_token)
                .await?;
            self.emit_instruction_warnings(warnings).await;
            loaded_agents_md
        });
        let prepare_tools = Box::pin(async {
            let extension_data =
                codex_extension_api::ExtensionData::new(turn_context.sub_id.clone());
            if let Some(messages) = settings
                .model_info
                .model_messages
                .as_ref()
                .and_then(|messages| messages.tools.as_ref())
                .and_then(|tools| tools.multi_agent.as_ref())
            {
                extension_data.insert(messages.clone());
            }
            // MCP refresh can be large; keep it off the sampling request's stack.
            let mcp =
                Box::pin(self.mcp_runtime_for_step(turn_context.as_ref(), required_servers)).await;
            let tool_router = turn::built_tools(
                self.as_ref(),
                turn_context.as_ref(),
                &settings.model_info,
                &environments,
                &mcp,
                &extension_data,
            )
            .await?;
            Ok::<_, CodexErr>((mcp, tool_router, extension_data))
        });
        // Returned warnings must finish delivery even if tools fail or preparation is cancelled.
        let (loaded_agents_md, prepared_tools) =
            tokio::join!(load_agents_md, prepare_tools.or_cancel(cancellation_token));
        let loaded_agents_md = loaded_agents_md?;
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let (mcp, tool_router, extension_data) = prepared_tools??;
        Ok(Arc::new(StepContext {
            preempt: turn_context
                .config
                .features
                .enabled(Feature::InstantInterrupt)
                .then(CancellationToken::new),
            settings,
            token_budget,
            session_telemetry,
            turn: turn_context,
            environments,
            extension_data,
            mcp,
            tool_router,
            loaded_agents_md,
        }))
    }

    pub(crate) async fn record_inter_agent_communication(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        communication: InterAgentCommunication,
    ) {
        let response_item = communication.to_model_input_item();
        let items = self
            .prepare_conversation_items_for_history(
                turn_context,
                model_info,
                std::slice::from_ref(&response_item),
            )
            .await;
        let items = items.as_ref();
        let mut response_item = ResponseItemEnvelope::new(items[0].clone());
        {
            let mut state = self.state.lock().await;
            response_item
                .metadata
                .get_or_insert_default()
                .user_input_order = Some(state.history.reserve_input_order());
            state.current_time_reminder.note_recorded_items(items);
            state.history.record_annotated_items(
                std::slice::from_mut(&mut response_item),
                model_info.truncation_policy.into(),
            );
        }
        self.persist_rollout_items(&[
            RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: communication.trigger_turn,
            },
            RolloutItem::ResponseItem(response_item),
        ])
        .await;
        self.send_raw_response_items(turn_context, items).await;
    }

    async fn maybe_warn_on_server_model_mismatch(
        self: &Arc<Self>,
        step_context: &StepContext,
        server_model: String,
    ) -> bool {
        let turn_context = &step_context.turn;
        let requested_model = step_context.settings.model_info.slug.clone();
        let server_model_normalized = server_model.to_ascii_lowercase();
        let requested_model_normalized = requested_model.to_ascii_lowercase();
        if server_model_normalized == requested_model_normalized {
            info!("server reported model {server_model} (matches requested model)");
            return false;
        }

        warn!("server reported model {server_model} while requested model was {requested_model}");

        let warning_message = format!(
            "Your account was flagged for potentially high-risk cyber activity and this request was routed to gpt-5.2 as a fallback. To regain access to gpt-5.3-codex, apply for trusted access: {CYBER_VERIFY_URL} or learn more: {CYBER_SAFETY_URL}"
        );

        self.send_event(
            turn_context,
            EventMsg::ModelReroute(ModelRerouteEvent {
                from_model: requested_model.clone(),
                to_model: server_model.clone(),
                reason: ModelRerouteReason::HighRiskCyberActivity,
            }),
        )
        .await;

        self.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: warning_message.clone(),
            }),
        )
        .await;
        true
    }

    pub(crate) async fn emit_model_verification(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        verifications: Vec<ModelVerification>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ModelVerification(ModelVerificationEvent { verifications }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_moderation_metadata(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        metadata: TurnModerationMetadataEvent,
    ) {
        self.send_event(turn_context, EventMsg::TurnModerationMetadata(metadata))
            .await;
    }

    pub(crate) async fn replace_compacted_history(
        &self,
        mut items: Vec<ResponseItemEnvelope>,
        reference_context_item: TurnContextItem,
        world_state_baseline: WorldStateSnapshot,
        metadata: CompactedHistoryMetadata,
    ) {
        for envelope in &mut items {
            Self::assign_missing_response_item_id(&mut envelope.item);
        }
        let mcp_revision = self
            .services
            .executed_tool_calls
            .mcp_attribution_checkpoint(/*force*/ true)
            .and_then(|(attribution, revision)| {
                items.last_mut().map(|envelope| {
                    envelope.metadata.get_or_insert_default().mcp_attribution = Some(attribution);
                    revision
                })
            });
        if let Some(checkpoint) = items.iter_mut().rev().find(|envelope| {
            matches!(
                envelope.item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            )
        }) {
            checkpoint
                .metadata
                .get_or_insert_default()
                .compaction_model_hash = metadata.compaction_model_hash;
        }
        // Wait for accepted updates to finish persisting, then keep later updates from
        // overtaking the current settings snapshot while its checkpoint is written.
        let _settings_guard = thread_settings::acquire_persistence_lock(self).await;
        let world_state_item = WorldStateItem::full(world_state_baseline.clone().into_object());
        let compacted_item = {
            let mut state = self.state.lock().await;
            // Goal edits are published outside the running task. Keep edits accepted after
            // the compaction input snapshot, in their original order, after its replacement.
            let replacement_goal_ids = crate::context::UserGoalUpdate::message_ids(
                items.iter().map(|envelope| &envelope.item),
            );
            items.extend(
                state
                    .history
                    .annotated_items()
                    .iter()
                    .filter(|envelope| {
                        crate::context::UserGoalUpdate::message_text(&envelope.item).is_some()
                            && envelope.item.id().is_some_and(|id| {
                                !metadata.input_goal_ids.contains(id)
                                    && !replacement_goal_ids.contains(id)
                            })
                    })
                    .cloned(),
            );
            let replacement_history = items.clone();
            state.replace_annotated_history(
                items,
                Some(reference_context_item.clone()),
                HistoryReplacement::Compaction,
            );
            state.reasoning_effort_pin = ReasoningEffortPin::Compacted;
            state.history.set_world_state_baseline(world_state_baseline);
            CompactedItem {
                message: metadata.message,
                replacement_history: Some(replacement_history),
                retained_context: Some(state.history.retained_context().clone()),
                window_number: Some(metadata.window_number),
                first_window_id: Some(metadata.window_ids.first_window_id.to_string()),
                previous_window_id: metadata
                    .window_ids
                    .previous_window_id
                    .map(|id| id.to_string()),
                window_id: Some(metadata.window_ids.window_id.to_string()),
                compaction_response_id: metadata.compaction_response_id,
                latest_token_usage_record: state.latest_token_usage_record.clone(),
                resume_metadata: Some(CompactionResumeMetadata {
                    multi_agent_version: self.multi_agent_version(),
                    last_started_turn_id: state.last_started_turn_id.clone(),
                    turn_attribution: state.turn_attribution.clone(),
                    previous_turn_settings: state.previous_turn_settings(),
                }),
            }
        };

        let rollout_items = [
            RolloutItem::Compacted(compacted_item),
            RolloutItem::WorldState(world_state_item),
            RolloutItem::TurnContext(reference_context_item),
            // The frozen turn context must not override current settings in persisted metadata.
            RolloutItem::EventMsg(thread_settings::applied_event(self).await),
        ];
        if self.persist_rollout_items(&rollout_items).await
            && let Some(revision) = mcp_revision
        {
            self.services
                .executed_tool_calls
                .mark_mcp_attribution_persisted(revision);
        }
        {
            let mut state = self.state.lock().await;
            state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
        }
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> Features {
        self.features.clone()
    }

    pub(crate) async fn collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state
            .session_configuration
            .step_settings
            .collaboration_mode
            .clone()
    }

    pub(crate) fn multi_agent_version(&self) -> Option<MultiAgentVersion> {
        self.multi_agent_version.get().copied()
    }

    pub(crate) fn set_multi_agent_version_if_unset(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> MultiAgentVersion {
        *self.multi_agent_version.get_or_init(|| multi_agent_version)
    }

    pub(crate) fn resolve_multi_agent_version_for_model(
        &self,
        model_info: &ModelInfo,
        config: &Config,
    ) -> MultiAgentVersion {
        if let Some(multi_agent_version) = self.multi_agent_version() {
            return config.multi_agent_version_for_model(Some(multi_agent_version));
        }

        let selected = config.multi_agent_version_for_model(model_info.multi_agent_version);

        self.set_multi_agent_version_if_unset(selected)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    async fn build_turn_context_contribution_items(
        &self,
        step_context: &StepContext,
    ) -> Vec<ResponseItem> {
        let turn_context = step_context.turn.as_ref();
        let mut developer_sections = Vec::new();
        let context_contributors = self.services.extensions.context_contributors().to_vec();

        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_turn_context(TurnContextContributionInput {
                    thread_id: self.thread_id(),
                    turn_id: turn_context.sub_id.as_str(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    model_context_window: step_context.settings.model_info.usable_context_window(),
                })
                .await
            {
                developer_sections.push(fragment.into());
            }
        }

        crate::context_manager::updates::build_rendered_message(developer_sections)
            .into_iter()
            .collect()
    }

    /// `step_context` and `world_state` must come from the same captured step.
    /// Returns rendered items with placement preserved for the caller's history layout.
    pub(crate) async fn build_initial_context_with_world_state(
        &self,
        step_context: &StepContext,
        world_state: &WorldState,
    ) -> (Vec<WorldStateUpdate>, WorldStateSnapshot) {
        let turn_context = step_context.turn.as_ref();
        let mut developer_sections = Vec::<RenderedFragment>::with_capacity(8);
        let mut contextual_user_sections = Vec::<RenderedFragment>::with_capacity(2);
        let mut separate_developer_sections = Vec::<RenderedFragment>::new();
        let mut context_window_hints = Vec::new();
        let (session_source, auto_compact_window_ids) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.session_source.clone(),
                state.auto_compact_window_ids(),
            )
        };
        if let Some(developer_instructions) = turn_context.developer_instructions.as_deref()
            && !developer_instructions.is_empty()
        {
            developer_sections
                .push(DeveloperInstructions::new(developer_instructions).render_fragment());
        }
        let context_contributors = self.services.extensions.context_contributors().to_vec();
        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_thread_context(
                    &self.services.session_extension_data,
                    &self.services.thread_extension_data,
                )
                .await
            {
                match fragment.slot() {
                    PromptSlot::ContextWindow => {
                        context_window_hints.push(fragment.text().to_string());
                    }
                    PromptSlot::DeveloperPolicy | PromptSlot::DeveloperCapabilities => {
                        developer_sections.push(fragment.into());
                    }
                }
            }
        }
        for contributor in &context_contributors {
            for fragment in contributor
                .contribute_turn_context(TurnContextContributionInput {
                    thread_id: self.thread_id(),
                    turn_id: turn_context.sub_id.as_str(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                    model_context_window: step_context.settings.model_info.usable_context_window(),
                })
                .await
            {
                developer_sections.push(fragment.into());
            }
        }
        // This is full-context metadata. Steady-state context diffs should not re-emit it.
        if turn_context.config.features.enabled(Feature::TokenBudget)
            && step_context
                .settings
                .model_info
                .resolved_context_window()
                .is_some()
        {
            // Keep the legacy bridge hint when native Notes is disabled. A failed
            // native request must not fall back to the bridge.
            if !turn_context
                .config
                .token_budget
                .as_ref()
                .is_some_and(|config| config.use_history_notes_extension)
                && let Some(mcp_result) = self
                    .services
                    .mcp_runtime
                    .latest_call_tool(
                        "notes",
                        "thread_hint",
                        /*arguments*/ None,
                        Some(serde_json::json!({
                            "threadId": self.thread_id().to_string(),
                        })),
                        /*requested_timeout*/ None,
                        /*wait_for_server*/ true,
                    )
                    .await
                    .ok()
                    .and_then(|result| {
                        let text = result
                            .content
                            .iter()
                            .filter_map(|content| {
                                content.get("text").and_then(serde_json::Value::as_str)
                            })
                            .filter(|text| !text.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        (!text.is_empty()).then_some(text)
                    })
            {
                context_window_hints.push(mcp_result);
            }
            separate_developer_sections.push(
                crate::context::TokenBudgetContext::new(
                    session_source
                        .get_agent_path()
                        .unwrap_or_else(codex_protocol::AgentPath::root),
                    auto_compact_window_ids.first_window_id,
                    auto_compact_window_ids.previous_window_id,
                    auto_compact_window_ids.window_id,
                    (!context_window_hints.is_empty()).then(|| context_window_hints.join("\n")),
                )
                .render_fragment(),
            );
        }
        let (snapshot, updates) = world_state.render_full();
        let (mut prefix, context) = split_prefix_updates(updates);
        let mut items = Vec::with_capacity(4);
        let mut context = context.into_iter();
        // Keep the initial-context grouping within each fragment run, but never across an item.
        loop {
            let mut next_item = None;
            // Render the active mode after the usage hint so it can override that hint.
            let mut initial_multi_agent_mode = None;
            for update in context.by_ref() {
                let fragment = match update.content {
                    WorldStateUpdateContent::Fragment(fragment) => fragment,
                    WorldStateUpdateContent::Item(item) => {
                        next_item = Some(*item);
                        break;
                    }
                };
                match fragment.role() {
                    "developer"
                        if fragment.markers().0 == ModelSwitchInstructions::type_markers().0 =>
                    {
                        // New-model instructions must precede the rest of the developer context.
                        developer_sections.insert(0, fragment.render_fragment());
                    }
                    "developer" if fragment.markers().0 == MULTI_AGENT_MODE_OPEN_TAG => {
                        initial_multi_agent_mode = Some(fragment);
                    }
                    "developer"
                        if fragment.markers().0 == MultiAgentRoleInstructions::type_markers().0 =>
                    {
                        separate_developer_sections.push(fragment.render_fragment());
                    }
                    "developer"
                        if update.placement == Placement::Standalone
                            && fragment.markers().0.is_empty() =>
                    {
                        separate_developer_sections.push(fragment.render_fragment());
                    }
                    "developer" => developer_sections.push(fragment.render_fragment()),
                    "user" => contextual_user_sections.push(fragment.render_fragment()),
                    _ => {}
                }
            }

            if let Some(developer_message) = crate::context_manager::updates::build_rendered_message(
                std::mem::take(&mut developer_sections),
            ) {
                items.push(developer_message);
            }
            for section in separate_developer_sections.drain(..) {
                if let Some(developer_message) =
                    crate::context_manager::updates::build_rendered_message(vec![section])
                {
                    items.push(developer_message);
                }
            }
            if let Some(initial_multi_agent_mode) = initial_multi_agent_mode
                && let Some(message) =
                    crate::context_manager::updates::build_rendered_message(vec![
                        initial_multi_agent_mode.render_fragment(),
                    ])
            {
                items.push(message);
            }
            if let Some(contextual_user_message) =
                crate::context_manager::updates::build_rendered_message(std::mem::take(
                    &mut contextual_user_sections,
                ))
            {
                items.push(contextual_user_message);
            }

            if let Some(item) = next_item {
                items.push(item);
            } else {
                break;
            }
        }
        // New context windows and compaction install these items directly into replacement history.
        for item in prefix.iter_mut().chain(&mut items) {
            item.set_turn_id_if_missing(&turn_context.sub_id);
        }
        let updates = prefix
            .into_iter()
            .map(WorldStateUpdate::prefix_item)
            .chain(items.into_iter().map(|item| WorldStateUpdate {
                placement: Placement::Standalone,
                content: WorldStateUpdateContent::Item(Box::new(item)),
            }))
            .collect();
        (updates, snapshot)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) -> bool {
        if let Some(live_thread) = self.live_thread()
            && let Err(e) = live_thread.append_items(items).await
        {
            error!("failed to record rollout items: {e:#}");
            return false;
        }
        true
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn conversation_history_snapshot(
        &self,
    ) -> Arc<dyn ConversationHistorySnapshot> {
        let state = self.state.lock().await;
        state.history.conversation_history_snapshot()
    }

    pub(crate) async fn current_window_id(&self) -> String {
        self.current_window().await.0
    }

    pub(crate) async fn current_window(&self) -> (String, u64, Uuid) {
        let state = self.state.lock().await;
        let thread_id = self.thread_id;
        let window_number = state.auto_compact_window_number();
        let context_window_id = state.auto_compact_window_ids().window_id;
        (
            format!("{thread_id}:{window_number}"),
            window_number,
            context_window_id,
        )
    }

    pub(crate) async fn advance_auto_compact_window(&self) -> (u64, AutoCompactWindowIds) {
        let mut state = self.state.lock().await;
        state.advance_auto_compact_window()
    }

    pub(crate) async fn request_new_context_window(&self) {
        let mut state = self.state.lock().await;
        state.request_new_context_window();
    }

    pub(crate) async fn take_new_context_window_request(&self) -> bool {
        let mut state = self.state.lock().await;
        state.take_new_context_window_request()
    }

    pub(crate) async fn start_new_context_window(
        &self,
        step_context: &StepContext,
        world_state: Arc<WorldState>,
    ) {
        let turn_context = step_context.turn.as_ref();
        let history = self.clone_history().await;
        let input_goal_ids = crate::context::UserGoalUpdate::message_ids(history.raw_items());
        let retained_client_items =
            crate::compact_remote_v2::truncate_retained_messages_for_remote_compaction(
                history
                    .annotated_items()
                    .iter()
                    .filter(|item| {
                        (self.enabled(Feature::RetainClientToolOutputs)
                            && crate::compact_retained_tool_outputs::is_retained_tool_output(item))
                            || (self.enabled(Feature::RetainClientDeveloperMessages)
                                && crate::compact_remote_v2::is_client_authored_developer_message(
                                    item,
                                ))
                    })
                    .cloned()
                    .collect(),
                crate::compact_remote_v2::RETAINED_MESSAGE_TOKEN_BUDGET,
            );
        let window = {
            let mut state = self.state.lock().await;
            state.start_new_context_window()
        };
        let (window_number, window_ids) = window;
        let (context_updates, world_state_snapshot) = self
            .build_initial_context_with_world_state(step_context, world_state.as_ref())
            .await;
        let context_items =
            crate::context_manager::updates::merge_world_state_updates(context_updates)
                .into_iter()
                .map(ResponseItemEnvelope::new)
                .chain(retained_client_items)
                .collect();
        let turn_context_item = step_context.to_turn_context_item();
        self.replace_compacted_history(
            context_items,
            turn_context_item,
            world_state_snapshot,
            CompactedHistoryMetadata {
                input_goal_ids,
                message: String::new(),
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
            },
        )
        .await;
        self.recompute_token_usage(turn_context).await;
    }

    pub(crate) async fn reference_context_item(&self) -> Option<TurnContextItem> {
        let state = self.state.lock().await;
        state.reference_context_item()
    }

    /// Persist the latest turn context snapshot for the first real user turn and for
    /// steady-state turns that emit model-visible context updates.
    ///
    /// When the reference snapshot is missing, this injects full initial context. Otherwise, it
    /// emits only context diffs.
    ///
    /// If full context is injected and a model switch occurred, this prepends the
    /// `<model_switch>` developer message so model-specific instructions are not lost.
    ///
    /// This is the normal runtime path that establishes a new `reference_context_item`.
    /// Compaction also establishes that reference when it installs full initial context
    /// into replacement history. Live world-state changes may
    /// independently advance their in-memory baseline within a turn.
    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn record_context_updates_and_set_reference_context_item(
        &self,
        step_context: &StepContext,
    ) -> CodexResult<()> {
        let turn_context = step_context.turn.as_ref();
        let reference_context_item = {
            let state = self.state.lock().await;
            state.reference_context_item()
        };
        let turn_context_item = step_context.to_turn_context_item();
        let turn_context_changed = reference_context_item.as_ref() != Some(&turn_context_item);
        let should_inject_full_context = reference_context_item.is_none();
        let world_state = self
            .build_world_state_for_step(step_context, should_inject_full_context)
            .await?;
        // Full initial context resets the baseline; later turns persist only its changes.
        let (mut context_items, world_state_item) = if should_inject_full_context {
            let (context_updates, snapshot) = self
                .build_initial_context_with_world_state(step_context, &world_state)
                .await;
            let context_items =
                crate::context_manager::updates::merge_world_state_updates(context_updates);
            self.state
                .lock()
                .await
                .history
                .set_world_state_baseline(snapshot.clone());
            (
                context_items,
                Some(WorldStateItem::full(snapshot.into_object())),
            )
        } else {
            let (world_state_items, world_state_item) = {
                let mut state = self.state.lock().await;
                let (fragments, rollout_item) = state.history.update_world_state(&world_state);
                (
                    crate::context_manager::updates::merge_world_state_updates(fragments),
                    rollout_item,
                )
            };
            (world_state_items, world_state_item)
        };
        if !should_inject_full_context && turn_context_changed {
            context_items.extend(
                self.build_turn_context_contribution_items(step_context)
                    .await,
            );
        }
        // A snapshot can change without producing model-visible or TurnContext updates.
        let only_world_state_changed = !turn_context_changed && context_items.is_empty();
        if only_world_state_changed && world_state_item.is_none() {
            return Ok(());
        }
        if !context_items.is_empty() {
            self.record_conversation_items(
                turn_context,
                &step_context.settings.model_info,
                &context_items,
            )
            .await;
        }
        // Persist state only after any model-visible context generated from it.
        if let Some(world_state_item) = world_state_item {
            self.persist_rollout_items(&[RolloutItem::WorldState(world_state_item)])
                .await;
        }
        // A snapshot-only change does not require a duplicate TurnContext record.
        if only_world_state_changed {
            return Ok(());
        }
        // Persist one `TurnContextItem` per real user turn so resume/lazy replay can recover the
        // latest durable baseline even when this turn emitted no model-visible context diffs.
        // Checkpoint current thread-owned settings after the first turn's context:
        // the step may predate an acknowledged update or use executor-owned permissions.
        // Serialize with settings updates so an older checkpoint cannot supersede a newer one.
        let _settings_guard = if should_inject_full_context {
            Some(thread_settings::acquire_persistence_lock(self).await)
        } else {
            None
        };
        let mut rollout_items = vec![RolloutItem::TurnContext(turn_context_item.clone())];
        if should_inject_full_context {
            rollout_items.push(RolloutItem::EventMsg(
                thread_settings::applied_event(self).await,
            ));
        }
        self.persist_rollout_items(&rollout_items).await;

        // Advance the persisted-settings baseline even when this turn emitted no model-visible
        // context items.
        let mut state = self.state.lock().await;
        state.set_reference_context_item(Some(turn_context_item));
        Ok(())
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        let result = self
            .record_token_usage_info(turn_context, &turn_context.initial_settings, token_usage)
            .await;
        self.send_token_count_event(turn_context).await;
        result
    }

    pub(crate) async fn record_observed_response_completed(
        &self,
        turn_context: &TurnContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::RawResponseCompleted(RawResponseCompletedEvent {
                response_id: response_id.to_string(),
                token_usage: usage.cloned(),
                usage_metadata: usage_metadata.cloned(),
            }),
        )
        .await;
        let Some(usage) = usage else {
            return;
        };
        let record = self.state.lock().await.record_token_usage(
            self.thread_id,
            &turn_context.sub_id,
            self.session_id(),
            turn_context
                .turn_metadata_state
                .root_turn_id()
                .unwrap_or_else(|| turn_context.sub_id.clone()),
            response_id.to_string(),
            usage,
        );
        self.persist_rollout_items(&[RolloutItem::TokenUsageRecord(record)])
            .await;
    }

    pub(crate) async fn record_token_usage_info(
        &self,
        turn_context: &TurnContext,
        settings: &ResolvedStepSettings,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        if let Some(token_usage) = token_usage {
            let token_info = {
                let mut state = self.state.lock().await;
                state
                    .update_token_info_from_usage(token_usage, turn_context.model_context_window());
                if matches!(
                    turn_context.config.model_auto_compact_token_limit_scope,
                    AutoCompactTokenLimitScope::BodyAfterPrefix
                ) {
                    state.ensure_auto_compact_window_server_prefill_from_usage(token_usage);
                }
                state.token_info()
            };
            let turn_state = self
                .input_queue
                .turn_state_for_sub_id(&self.active_turn, &turn_context.sub_id)
                .await;
            if let Some(turn_state) = turn_state {
                turn_state.lock().await.token_usage_by_model.record(
                    settings.selected_collaboration_mode().model(),
                    settings.telemetry(&turn_context.session_telemetry),
                    token_usage,
                );
            }
            let budget_result = self.record_rollout_budget_usage(token_usage).await;
            if let Some(token_info) = token_info.as_ref() {
                for contributor in self.services.extensions.token_usage_contributors() {
                    contributor
                        .on_token_usage(
                            &self.services.session_extension_data,
                            &self.services.thread_extension_data,
                            turn_context.extension_data.as_ref(),
                            token_info,
                        )
                        .await;
                }
            }
            budget_result?;
        }
        Ok(())
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let history = self.clone_history().await;
        let base_instructions = self.get_base_instructions().await;
        let Some(estimated_total_tokens) =
            history.estimate_token_count_with_base_instructions(&base_instructions)
        else {
            return;
        };
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_total_tokens.max(0),
                codex_rollout_budget_units: None,
            };

            if let Some(model_context_window) = turn_context.model_context_window() {
                info.model_context_window = Some(model_context_window);
            }

            state.set_token_info(Some(info));
        }
        self.set_auto_compact_window_estimated_prefill_for_scope(
            turn_context,
            estimated_total_tokens,
        )
        .await;
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        self.record_rate_limits_info(new_rate_limits).await;
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_rate_limits_info(&self, new_rate_limits: RateLimitSnapshot) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
    }

    pub(crate) async fn set_server_reasoning_included(&self, included: bool) {
        let mut state = self.state.lock().await;
        state.set_server_reasoning_included(included);
    }

    pub(crate) async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(
            turn_context,
            model_info,
            std::slice::from_ref(&response_item),
        )
        .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn record_user_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        model_info: &ModelInfo,
        input: &[UserInput],
        client_id: Option<String>,
        metadata: UserInputMetadata,
    ) {
        // Persist the user message to history, but emit the turn item from `UserInput` so
        // UI-only `text_elements` are preserved. `ResponseItem::Message` does not carry
        // those spans, and `record_response_item_and_emit_turn_item` would drop them.
        let mut user_image_content_indices = HashMap::new();
        let mut response_item = self.response_item_from_user_input_with_image_positions(
            input.to_vec(),
            &mut user_image_content_indices,
        );
        if metadata.origin == codex_history::UserInputOrigin::Heartbeat
            && let ResponseItem::Message {
                content,
                internal_chat_message_metadata_passthrough: Some(metadata),
                ..
            } = &mut response_item
            && matches!(content.as_slice(), [ContentItem::InputText { .. }])
        {
            metadata.content_item_kinds = Some(vec![ContentItemKind(
                codex_history::HEARTBEAT_CONTENT_KIND.to_owned(),
            )]);
        }
        let prepared_items = self
            .prepare_annotated_conversation_items_for_history(
                turn_context,
                model_info,
                vec![ResponseItemEnvelope {
                    item: response_item,
                    metadata: metadata.acceptance_order.map(|order| CodexHarnessMetadata {
                        user_input_order: Some(order),
                        ..Default::default()
                    }),
                }],
            )
            .await;
        let mut user_message_item = UserMessageItem::new(input);
        apply_prepared_image_file_ids(
            &mut user_message_item,
            &prepared_items,
            &user_image_content_indices,
        );
        self.record_prepared_conversation_items(turn_context, model_info, prepared_items)
            .await;
        user_message_item.client_id = client_id;
        let turn_item = TurnItem::UserMessage(user_message_item);
        self.emit_turn_item_started(turn_context, &turn_item).await;
        self.emit_turn_item_completed(turn_context, turn_item).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let had_active_turn = self.active_turn.lock().await.is_some();
        self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        if !had_active_turn {
            self.cancel_mcp_startup();
        }
    }

    pub(crate) fn hooks(&self) -> Arc<Hooks> {
        self.services.hooks.load_full()
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    pub(crate) async fn take_pending_session_start_source(
        &self,
    ) -> Option<codex_hooks::SessionStartSource> {
        let mut state = self.state.lock().await;
        state.take_pending_session_start_source()
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }
}

fn apply_prepared_image_file_ids(
    user_message_item: &mut UserMessageItem,
    prepared_items: &[ResponseItemEnvelope],
    user_image_content_indices: &HashMap<usize, usize>,
) {
    // Preparation replaces image slots in place; resize notices are separate messages.
    // Read the references from the original message while retaining UI-only input spans.
    if let Some(ResponseItemEnvelope {
        item: ResponseItem::Message { content, .. },
        ..
    }) = prepared_items.first()
    {
        for (&input_index, &content_index) in user_image_content_indices {
            if let Some(ContentItem::InputImage {
                image: ImageReference::File { file_id },
                ..
            }) = content.get(content_index)
                && let UserInput::Image { detail, .. } | UserInput::LocalImage { detail, .. } =
                    &user_message_item.content[input_index]
            {
                user_message_item.content[input_index] = UserInput::Image {
                    image: ImageReference::File {
                        file_id: file_id.clone(),
                    },
                    detail: *detail,
                };
            }
        }
    }
}

/// Builds hook configuration for one config snapshot.
fn build_hooks_config(config: &Config, environment: Option<&TurnEnvironment>) -> HooksConfig {
    let (hook_shell_program, hook_shell_argv) = environment
        .and_then(|environment| environment.shell.as_ref())
        .map(|shell| {
            let mut argv = shell.derive_exec_args("", /*use_login_shell*/ false);
            let program = argv.remove(0);
            let _ = argv.pop();
            (Some(program), argv)
        })
        .unwrap_or_default();
    HooksConfig {
        legacy_notify_argv: config.notify.clone(),
        feature_enabled: config.features.enabled(Feature::CodexHooks),
        config_layer_stack: Some(config.config_layer_stack.clone()),
        shell_program: hook_shell_program,
        shell_args: hook_shell_argv,
    }
}

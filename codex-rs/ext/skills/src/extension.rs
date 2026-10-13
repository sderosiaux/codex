use std::sync::Arc;

use crate::HostSkillsSnapshot;
use crate::InjectedHostSkillPrompts;
use codex_extension_api::ConfigContributor;
use codex_extension_api::ContextContributor;
use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::PromptFragment;
use codex_extension_api::SkillInvocationContributor;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_protocol::openai_models::ModelInfo;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillReadResult;
use crate::fragments::AvailableSkillsInstructions;
use crate::fragments::SkillInstructions;
use crate::provider::HostSkillProvider;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadRequest;
use crate::render::AvailableSkillsRender;
use crate::render::MAX_SKILL_NAME_BYTES;
use crate::render::MAX_SKILL_PATH_BYTES;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::SkillMetadataBudget;
use crate::render::SkillRenderReport;
use crate::render::render_available_skills;
use crate::render::skill_metadata_budget;
use crate::render::truncate_main_prompt_contents;
use crate::render::truncate_utf8_to_bytes;
use crate::render_observability::CatalogSurface;
use crate::render_observability::record_catalog_render;
use crate::selection::collect_explicit_skill_mentions;
use crate::sources::SkillProviders;
use crate::state::HostSkillsCatalogInWorldState;
use crate::state::HostSkillsStepState;
use crate::state::SkillsSessionState;
use crate::state::SkillsThreadState;
use crate::state::SkillsTurnState;
use crate::telemetry::SkillTelemetry;
use crate::warnings::bounded_warnings;
use crate::world_state_catalogs::CatalogContext;

struct SkillsExtension<C> {
    providers: SkillProviders,
    event_sink: Arc<dyn ExtensionEventSink>,
    config_from_host: Arc<dyn Fn(&C) -> SkillsExtensionConfig + Send + Sync>,
}

#[derive(Default)]
struct RenderedCatalog {
    fragment: Option<AvailableSkillsInstructions>,
    warning_message: Option<String>,
}

fn render_catalog(
    extension_metrics: Option<&dyn ExtensionMetrics>,
    catalog_surface: CatalogSurface,
    catalog: &SkillCatalog,
    include_skills_usage_instructions: bool,
    policy: SkillCatalogRenderPolicy,
    budget: SkillMetadataBudget,
) -> RenderedCatalog {
    render_prepared_catalog(
        extension_metrics,
        catalog_surface,
        include_skills_usage_instructions,
        budget,
        render_available_skills(catalog, policy, budget),
    )
}

fn render_prepared_catalog(
    extension_metrics: Option<&dyn ExtensionMetrics>,
    catalog_surface: CatalogSurface,
    include_skills_usage_instructions: bool,
    budget: SkillMetadataBudget,
    rendered: Option<AvailableSkillsRender>,
) -> RenderedCatalog {
    let Some(rendered) = rendered else {
        record_catalog_render(
            extension_metrics,
            catalog_surface,
            budget,
            &SkillRenderReport::default(),
        );
        return RenderedCatalog::default();
    };
    record_catalog_render(extension_metrics, catalog_surface, budget, &rendered.report);
    let warning_message = rendered.report.warning_message();
    let fragment = rendered.into_fragment(include_skills_usage_instructions);
    RenderedCatalog {
        fragment,
        warning_message,
    }
}

impl<C> ThreadLifecycleContributor<C> for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.session_store.insert(SkillsSessionState {
                extension_metrics: input.extension_metrics.clone(),
            });
            input
                .thread_store
                .insert(SkillsThreadState::new((self.config_from_host)(
                    input.config,
                )));
        })
    }
}

impl<C> ConfigContributor<C> for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_config_changed(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
        _previous_config: &C,
        new_config: &C,
    ) {
        let next_config = (self.config_from_host)(new_config);
        if let Some(state) = thread_store.get::<SkillsThreadState>() {
            state.set_config(next_config);
        } else {
            thread_store.insert(SkillsThreadState::new(next_config));
        }
    }
}

impl<C> ContextContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute_thread_context<'a>(
        &'a self,
        session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<PromptFragment>> + Send + 'a>> {
        Box::pin(async move {
            let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
                return Vec::new();
            };
            let config = thread_state.config();
            if !config.include_instructions {
                return Vec::new();
            }
            let catalog = self
                .providers
                .list_for_turn(SkillListQuery {
                    turn_id: thread_store.level_id().to_string(),
                    host_snapshot: None,
                    include_host_skills: false,
                })
                .await;
            for warning in bounded_warnings(&catalog.warnings) {
                self.emit_warning(thread_store.level_id(), /*turn_id*/ None, warning);
            }
            let include_usage = thread_store
                .get::<ModelInfo>()
                .is_some_and(|model_info| model_info.include_skills_usage_instructions);
            let extension_metrics = session_store
                .get::<SkillsSessionState>()
                .and_then(|state| state.extension_metrics.clone());
            let rendered = render_catalog(
                extension_metrics.as_deref(),
                CatalogSurface::ThreadContext,
                &catalog,
                include_usage,
                SkillCatalogRenderPolicy::ExtensionCompatible,
                skill_metadata_budget(/*context_window*/ None, config.max_context_tokens),
            );
            if let Some(message) = rendered.warning_message {
                self.emit_warning(thread_store.level_id(), /*turn_id*/ None, message);
            }
            rendered
                .fragment
                .map(|fragment| {
                    PromptFragment::developer_capability(fragment.render(), fragment.content_kind())
                })
                .into_iter()
                .collect()
        })
    }

    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let Some(context) =
                CatalogContext::new(&self.providers, Arc::clone(&self.event_sink), input)
            else {
                return Vec::new();
            };
            let Some(catalog) = context.discover_host_catalog().await else {
                return Vec::new();
            };
            vec![context.build_world_state_section(&catalog)]
        })
    }
}

impl<C> SkillInvocationContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn requires_host_skill_discovery(&self) -> bool {
        self.providers.has_host_provider()
    }
}

impl<C> TurnInputContributor for SkillsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(thread_state) = thread_store.get::<SkillsThreadState>() else {
                return Vec::new();
            };

            let config = thread_state.config();
            let host_snapshot = turn_store.get::<HostSkillsSnapshot>();
            let host_catalog_in_world_state =
                turn_store.get::<HostSkillsCatalogInWorldState>().is_some();
            let host_skills = turn_store.get::<HostSkillsStepState>();
            let query = SkillListQuery {
                turn_id: input.turn_id.clone(),
                host_snapshot: host_snapshot.clone(),
                include_host_skills: host_skills.is_none() && !host_catalog_in_world_state,
            };
            let catalog = self.providers.list_for_turn(query).await;
            for warning in bounded_warnings(&catalog.warnings) {
                self.emit_warning(thread_store.level_id(), Some(&input.turn_id), warning);
            }

            let selected_entries = collect_explicit_skill_mentions(&input.user_input, &catalog);
            let mut fragments: Vec<Box<dyn ContextualUserFragment + Send>> = Vec::new();
            if config.include_instructions && !host_catalog_in_world_state {
                let model_info = thread_store.get::<ModelInfo>();
                let include_usage = model_info
                    .as_deref()
                    .is_some_and(|model_info| model_info.include_skills_usage_instructions);
                let context_window = model_info
                    .as_deref()
                    .and_then(ModelInfo::resolved_context_window);
                let metadata_budget =
                    skill_metadata_budget(context_window, config.max_context_tokens);
                let rendered = render_catalog(
                    extension_metrics.as_deref(),
                    CatalogSurface::TurnInput,
                    &catalog,
                    include_usage,
                    SkillCatalogRenderPolicy::ExtensionCompatible,
                    metadata_budget,
                );
                if let Some(message) = rendered.warning_message {
                    self.emit_warning(thread_store.level_id(), Some(&input.turn_id), message);
                }
                if let Some(fragment) = rendered.fragment {
                    fragments.push(Box::new(fragment));
                }
            }

            let mut warnings = catalog.warnings.clone();
            let mut main_prompts_injected = false;
            let mut injected_host_skill_prompts = InjectedHostSkillPrompts::default();
            for entry in &selected_entries {
                match self.read_main_prompt(entry, host_snapshot.clone()).await {
                    Ok(read_result) => {
                        let (contents, truncated) =
                            truncate_main_prompt_contents(read_result.contents.as_str());
                        if truncated {
                            let warning = format!(
                                "Skill `{}` exceeded the main prompt context limit and was truncated.",
                                entry.name
                            );
                            self.emit_warning(
                                thread_store.level_id(),
                                Some(&input.turn_id),
                                warning.clone(),
                            );
                            warnings.push(warning);
                        }
                        let fragment = SkillInstructions {
                            name: truncate_utf8_to_bytes(&entry.name, MAX_SKILL_NAME_BYTES).0,
                            path: truncate_utf8_to_bytes(
                                entry.rendered_path(),
                                MAX_SKILL_PATH_BYTES,
                            )
                            .0,
                            contents,
                        };
                        fragments.push(Box::new(fragment));
                        main_prompts_injected = true;
                        injected_host_skill_prompts.insert_path(entry.main_prompt.as_str());
                    }
                    Err(message) => {
                        let warning = format!("Failed to load skill `{}`: {message}", entry.name);
                        self.emit_warning(
                            thread_store.level_id(),
                            Some(&input.turn_id),
                            warning.clone(),
                        );
                        warnings.push(warning);
                    }
                }
            }

            turn_store.insert(SkillsTurnState {
                catalog,
                selected_entries,
                warnings,
                main_prompts_injected,
            });
            if !injected_host_skill_prompts.is_empty() {
                turn_store.insert(injected_host_skill_prompts);
            }

            fragments
        })
    }
}

impl<C> SkillsExtension<C> {
    #[tracing::instrument(level = "trace", skip_all, fields(skill = %entry.name))]
    async fn read_main_prompt(
        &self,
        entry: &SkillCatalogEntry,
        host_snapshot: Option<Arc<HostSkillsSnapshot>>,
    ) -> Result<SkillReadResult, String> {
        self.providers
            .read(SkillReadRequest {
                authority: entry.authority.clone(),
                resource: entry.main_prompt.clone(),
                host_snapshot,
            })
            .await
            .map_err(|err| err.message)
    }

    fn emit_warning(&self, thread_id: &str, turn_id: Option<&str>, message: String) {
        self.event_sink.emit_warning(ExtensionWarning {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            message,
        });
    }
}

pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    config_from_host: impl Fn(&C) -> SkillsExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    install_with_providers(
        registry,
        SkillProviders::new().with_host_provider(Arc::new(HostSkillProvider::new())),
        config_from_host,
    );
}

pub fn install_with_providers<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    providers: SkillProviders,
    config_from_host: impl Fn(&C) -> SkillsExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(SkillsExtension {
        providers,
        event_sink: registry.event_sink(),
        config_from_host: Arc::new(config_from_host),
    });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.turn_lifecycle_contributor(Arc::new(SkillTelemetry));
    registry.config_contributor(extension.clone());
    registry.prompt_contributor(extension.clone());
    registry.turn_input_contributor(extension.clone());
    registry.skill_invocation_contributor(extension);
}

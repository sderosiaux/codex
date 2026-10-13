use std::sync::Arc;

use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionEventSink;
use codex_extension_api::ExtensionWarning;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;

use crate::HostSkillsSnapshot;
use crate::SkillsExtensionConfig;
use crate::catalog::SkillCatalog;
use crate::provider::SkillListQuery;
use crate::render::SkillCatalogRenderPolicy;
use crate::render::SkillMetadataBudget;
use crate::render::render_available_skills;
use crate::render::skill_metadata_budget;
use crate::render_observability::trace_catalog_budget_pressure;
use crate::sources::SkillProviders;
use crate::state::EmittedCatalogBudgetWarnings;
use crate::state::HostSkillsCatalogInWorldState;
use crate::state::HostSkillsStepState;
use crate::state::SkillsThreadState;
use crate::world_state::CatalogRenderCallback;
use crate::world_state::host_skills_world_state_section;

type CatalogWarningEmitter = Arc<dyn Fn(String) + Send + Sync>;

pub(crate) struct CatalogContext<'a> {
    providers: &'a SkillProviders,
    input: WorldStateContributionInput<'a>,
    config: SkillsExtensionConfig,
    metadata_budget: SkillMetadataBudget,
    include_usage: bool,
    warning_emitter: CatalogWarningEmitter,
}

impl<'a> CatalogContext<'a> {
    pub(crate) fn new(
        providers: &'a SkillProviders,
        event_sink: Arc<dyn ExtensionEventSink>,
        input: WorldStateContributionInput<'a>,
    ) -> Option<Self> {
        let thread_state = input.thread_store.get::<SkillsThreadState>()?;
        let config = thread_state.config();
        let include_usage = input.model_info.include_skills_usage_instructions;
        let context_window = input.model_info.resolved_context_window();
        let metadata_budget = skill_metadata_budget(context_window, config.max_context_tokens);
        let emitted_warnings = input
            .turn_store
            .get_or_init(EmittedCatalogBudgetWarnings::default);
        let thread_id = input.thread_store.level_id().to_string();
        let turn_id = input.turn_id.to_string();
        let warning_emitter: CatalogWarningEmitter = Arc::new(move |message| {
            if emitted_warnings.insert(&message) {
                event_sink.emit_warning(ExtensionWarning {
                    thread_id: thread_id.clone(),
                    turn_id: Some(turn_id.clone()),
                    message,
                });
            }
        });

        Some(Self {
            providers,
            input,
            config,
            metadata_budget,
            include_usage,
            warning_emitter,
        })
    }

    /// Lists the host catalog, or `None` when host skills are unavailable for this turn.
    pub(crate) async fn discover_host_catalog(&self) -> Option<SkillCatalog> {
        let host_snapshot = self
            .input
            .turn_store
            .get::<HostSkillsSnapshot>()
            .filter(|_| self.providers.has_host_provider())?;
        if !self.config.include_instructions {
            return Some(SkillCatalog::default());
        }
        let catalog = self
            .providers
            .list_host_for_turn(SkillListQuery {
                turn_id: self.input.turn_id.to_string(),
                host_snapshot: Some(host_snapshot),
                include_host_skills: true,
            })
            .await;
        self.input.turn_store.insert(HostSkillsStepState);
        Some(catalog)
    }

    pub(crate) fn build_world_state_section(
        &self,
        catalog: &SkillCatalog,
    ) -> WorldStateSectionContribution {
        let include_instructions = self.config.include_instructions;
        let rendered = include_instructions
            .then(|| {
                render_available_skills(
                    catalog,
                    SkillCatalogRenderPolicy::CoreCompatible,
                    self.metadata_budget,
                )
            })
            .flatten();
        let budget = self.metadata_budget;
        let report = rendered
            .as_ref()
            .map(|rendered| rendered.report.clone())
            .unwrap_or_default();
        let body = rendered
            .and_then(|rendered| rendered.into_fragment(self.include_usage))
            .map(|fragment| fragment.body());
        let warning_emitter = Arc::clone(&self.warning_emitter);
        let render_report = report.clone();
        let on_render: CatalogRenderCallback = Box::new(move || {
            if !include_instructions {
                return;
            }

            trace_catalog_budget_pressure(budget, &render_report);
            if let Some(message) = render_report.warning_message() {
                warning_emitter(message);
            }
        });

        self.input.turn_store.insert(HostSkillsCatalogInWorldState);
        host_skills_world_state_section(body, include_instructions, &report, on_render)
    }
}

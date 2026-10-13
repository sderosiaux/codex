//! Configured inputs retained independently of future thread settings.

use codex_features::Features;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use std::sync::Arc;

/// Model and execution settings selected for an individual model step within
/// a turn. A turn may contain several steps, each using its own captured settings.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StepSettings {
    pub(crate) collaboration_mode: CollaborationMode,
    /// `None` follows the pinned model's default.
    pub(crate) reasoning_summary: Option<ReasoningSummary>,
    /// Normalized requested tier. Startup retains initial-model filtering;
    /// resolution filters the effective tier against the pinned model.
    pub(crate) service_tier: Option<String>,
    pub(crate) personality: Option<Personality>,
}

/// Immutable configured choices, pinned model metadata, and effective request values.
///
/// One snapshot may produce several fresh `StepContext`s. Replacing the
/// snapshot does not change steps or actions that have already captured it.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedStepSettings {
    /// Inputs for later sparse patches. Unset defaults and unsupported requested
    /// tiers must not be reconstructed from the effective values below.
    selected: Arc<StepSettings>,
    pub(crate) model_info: Arc<ModelInfo>,
    /// Effective request summary: the configured value or the pinned model's default.
    pub(crate) reasoning_summary: ReasoningSummary,
    /// Tier sent on requests, filtered by feature and model support.
    pub(crate) service_tier: Option<String>,
}

impl ResolvedStepSettings {
    /// Derives request values from selected settings and pinned metadata.
    pub(super) fn new(
        selected: Arc<StepSettings>,
        model_info: Arc<ModelInfo>,
        features: &Features,
    ) -> Self {
        let reasoning_summary = selected
            .reasoning_summary
            .unwrap_or(model_info.default_reasoning_summary);
        let service_tier =
            super::get_service_tier(selected.service_tier.clone(), features, &model_info);
        Self {
            selected,
            model_info,
            reasoning_summary,
            service_tier,
        }
    }

    pub(crate) fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.selected
            .collaboration_mode
            .settings
            .reasoning_effort
            .as_ref()
    }

    pub(crate) fn effective_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort()
            .or(self.model_info.default_reasoning_level.as_ref())
            .cloned()
    }

    pub(crate) fn effective_collaboration_mode(&self) -> CollaborationMode {
        let mut collaboration_mode = self.selected.collaboration_mode.clone();
        collaboration_mode.settings.model = self.model_info.slug.clone();
        collaboration_mode
    }

    /// Retained inputs for constructing a snapshot against different model metadata.
    pub(super) fn selected(&self) -> &StepSettings {
        &self.selected
    }

    pub(super) fn selected_collaboration_mode(&self) -> &CollaborationMode {
        &self.selected.collaboration_mode
    }

    /// Applies sparse edits to the retained selection, preserving pinned metadata
    /// unless the model changes, then resolves request values.
    pub(super) async fn apply_update(
        &self,
        update: &StepSettingsUpdate,
        models_manager: &dyn ModelsManager,
        overrides: &ModelInfoOverrides,
        features: &Features,
    ) -> Self {
        let selected = self.selected.apply(update);
        let model_info =
            if selected.collaboration_mode.model() == self.selected.collaboration_mode.model() {
                Arc::clone(&self.model_info)
            } else {
                Arc::new(selected.resolve_model_info(models_manager, overrides).await)
            };
        Self::new(Arc::new(selected), model_info, features)
    }
}

/// Explicit startup overrides applied to catalog-derived model metadata.
/// Construct from `Config::to_models_manager_config()` so model-derived base
/// instructions are not mistaken for explicit overrides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelInfoOverrides {
    pub(crate) context_window: Option<i64>,
    pub(crate) auto_compact_token_limit: Option<i64>,
    pub(crate) tool_output_token_limit: Option<usize>,
    pub(crate) base_instructions: Option<String>,
}

impl From<ModelsManagerConfig> for ModelInfoOverrides {
    fn from(config: ModelsManagerConfig) -> Self {
        Self {
            context_window: config.model_context_window,
            auto_compact_token_limit: config.model_auto_compact_token_limit,
            tool_output_token_limit: config.tool_output_token_limit,
            base_instructions: config.base_instructions,
        }
    }
}

impl ModelInfoOverrides {
    pub(crate) fn models_manager_config(
        &self,
        personality: Option<Personality>,
    ) -> ModelsManagerConfig {
        ModelsManagerConfig {
            model_context_window: self.context_window,
            model_auto_compact_token_limit: self.auto_compact_token_limit,
            tool_output_token_limit: self.tool_output_token_limit,
            base_instructions: self.base_instructions.clone(),
            personality,
            // The models manager already owns its catalog.
            model_catalog: None,
        }
    }
}

/// Sparse edits applied independently to each settings owner.
///
/// Do not materialize a partial update against one settings owner and reuse
/// that full value for another. Merge the requested edits with each target.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StepSettingsUpdate {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<Option<ReasoningEffort>>,
    /// A complete collaboration mode takes precedence over model and effort edits.
    pub(crate) collaboration_mode: Option<CollaborationMode>,
    pub(crate) reasoning_summary: Option<ReasoningSummary>,
    pub(crate) service_tier: Option<Option<String>>,
    pub(crate) personality: Option<Personality>,
}

impl StepSettings {
    /// Resolves the selected model using the session's explicit startup overrides.
    pub(super) async fn resolve_model_info(
        &self,
        models_manager: &dyn ModelsManager,
        overrides: &ModelInfoOverrides,
    ) -> ModelInfo {
        let config = overrides.models_manager_config(self.personality);
        models_manager
            .get_model_info(self.collaboration_mode.model(), &config)
            .await
    }

    /// Applies sparse edits to these settings.
    pub(crate) fn apply(&self, update: &StepSettingsUpdate) -> Self {
        let mut next = self.clone();
        next.collaboration_mode = update.collaboration_mode.clone().unwrap_or_else(|| {
            self.collaboration_mode.with_updates(
                update.model.clone(),
                update.effort.clone(),
                /*developer_instructions*/ None,
            )
        });
        if let Some(summary) = update.reasoning_summary {
            next.reasoning_summary = Some(summary);
        }
        if let Some(service_tier) = update.service_tier.clone() {
            // TODO(aibrahim): Remove once v2 clients no longer send the legacy
            // "fast" service tier value.
            next.service_tier = Some(match service_tier {
                Some(service_tier) => ServiceTier::from_request_value(&service_tier)
                    .map_or(service_tier, |service_tier| {
                        service_tier.request_value().to_string()
                    }),
                None => SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string(),
            });
        }
        if let Some(personality) = update.personality {
            next.personality = Some(personality);
        }
        next
    }
}

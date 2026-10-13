use super::*;
use crate::config::test_config;
use codex_models_manager::manager::StaticModelsManager;
use codex_models_manager::model_info::model_info_from_slug;
use codex_prompts::render_model_instructions;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::BaseInstructionsProvenance;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::TruncationPolicyConfig;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

pub(crate) fn update_selected_settings_for_test(
    settings: &mut ResolvedStepSettings,
    update: impl FnOnce(&mut StepSettings),
) {
    update(Arc::make_mut(&mut settings.selected));
}

fn configured_settings() -> StepSettings {
    StepSettings {
        collaboration_mode: CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: "model-a".to_string(),
                reasoning_effort: Some(ReasoningEffort::Low),
                developer_instructions: Some("keep these instructions".to_string()),
            },
        },
        reasoning_summary: Some(ReasoningSummary::Concise),
        service_tier: None,
        personality: Some(Personality::Friendly),
    }
}

#[test]
fn sparse_patch_uses_the_settings_version_being_updated() {
    let initial = configured_settings();
    let tier_update = StepSettingsUpdate {
        service_tier: Some(Some("fast".to_string())),
        ..Default::default()
    };
    let latest = initial.apply(&StepSettingsUpdate {
        model: Some("model-b".to_string()),
        effort: Some(Some(ReasoningEffort::High)),
        ..Default::default()
    });

    let updated = latest.apply(&tier_update);
    let expected = StepSettings {
        service_tier: Some(ServiceTier::Fast.request_value().to_string()),
        ..latest
    };
    assert_eq!(updated, expected);
}

#[test]
fn collaboration_replacement_wins_and_effort_clear_remains_sparse() {
    let initial = configured_settings();
    let replacement = CollaborationMode {
        mode: ModeKind::Plan,
        settings: Settings {
            model: "model-b".to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
            developer_instructions: None,
        },
    };
    let replaced = initial.apply(&StepSettingsUpdate {
        model: Some("ignored-model".to_string()),
        effort: Some(Some(ReasoningEffort::High)),
        collaboration_mode: Some(replacement.clone()),
        ..Default::default()
    });
    assert_eq!(
        replaced,
        StepSettings {
            collaboration_mode: replacement.clone(),
            ..initial
        }
    );

    let cleared = replaced.apply(&StepSettingsUpdate {
        effort: Some(None),
        ..Default::default()
    });
    assert_eq!(
        cleared,
        StepSettings {
            collaboration_mode: replacement.with_updates(
                /*model*/ None,
                Some(None),
                /*developer_instructions*/ None,
            ),
            ..replaced
        }
    );
}

#[test_case(BaseInstructionsProvenance::Custom; "explicit instructions")]
#[test_case(BaseInstructionsProvenance::Model { model: "model-a".to_string() }; "model-derived instructions")]
#[tokio::test]
async fn model_resolution_preserves_startup_overrides_and_instruction_provenance(
    provenance: BaseInstructionsProvenance,
) {
    let configured_instructions = "explicit instructions";
    let mut model = model_info_from_slug("model-b");
    model.context_window = Some(90_000);
    model.max_context_window = Some(100_000);
    model.auto_compact_token_limit = Some(80_000);
    model.truncation_policy = TruncationPolicyConfig::tokens(/*limit*/ 1_000);
    let messages = model
        .model_messages
        .as_mut()
        .expect("test model should have instruction metadata");
    messages.instructions_template =
        Some("Catalog B.\n# Personality\nfixed\n# Rules\nKeep the rules.".to_string());
    let catalog = ModelsResponse {
        models: vec![model],
    };
    let models_manager = StaticModelsManager::new(/*auth_manager*/ None, catalog.clone());

    let mut config = test_config().await;
    config.model = Some("model-a".to_string());
    config.model_catalog = Some(catalog);
    config.model_context_window = Some(160_000);
    config.model_auto_compact_token_limit = Some(70_000);
    config.tool_output_token_limit = Some(777);
    config.base_instructions = Some(configured_instructions.to_string());
    let explicit_instructions = matches!(&provenance, BaseInstructionsProvenance::Custom);
    config.base_instructions_provenance = Some(provenance);

    // Capture the same filtered explicit overrides that session startup owns.
    let overrides = ModelInfoOverrides::from(config.to_models_manager_config());

    for (personality, catalog_instructions) in [
        (
            Personality::Friendly,
            "Catalog B.\n# Personality\nfixed\n# Rules\nKeep the rules.",
        ),
        (Personality::None, "Catalog B.\n# Rules\nKeep the rules."),
    ] {
        config.personality = Some(personality);
        let mut settings = configured_settings();
        settings.collaboration_mode = settings.collaboration_mode.with_updates(
            Some("model-b".to_string()),
            /*effort*/ None,
            /*developer_instructions*/ None,
        );
        settings.personality = config.personality;

        let legacy = models_manager
            .get_model_info("model-b", &config.to_models_manager_config())
            .await;
        let resolved = settings
            .resolve_model_info(&models_manager, &overrides)
            .await;
        assert_eq!(resolved, legacy);
        assert_eq!(
            render_model_instructions(&resolved),
            if explicit_instructions {
                configured_instructions
            } else {
                catalog_instructions
            },
        );
    }
}

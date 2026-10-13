use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID;
use codex_models_manager::bundled_models_response;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::WebSearchToolType;

use super::BedrockEndpoint;

const GPT_5_BEDROCK_CONTEXT_WINDOW: i64 = 272_000;
const GPT_5_6_SOL_OPENAI_MODEL_ID: &str = "gpt-5.6-sol";
const GPT_5_6_TERRA_OPENAI_MODEL_ID: &str = "gpt-5.6-terra";
const GPT_5_6_LUNA_OPENAI_MODEL_ID: &str = "gpt-5.6-luna";
const GPT_6_1_SOL_OPENAI_MODEL_ID: &str = "gpt-6.1-sol";
const GPT_6_SOL_OPENAI_MODEL_ID: &str = "gpt-6-sol";
const GPT_6_LUNA_OPENAI_MODEL_ID: &str = "gpt-6-luna";
const GPT_6_ASTRA_OPENAI_MODEL_ID: &str = "gpt-6-astra";
const GPT_5_5_OPENAI_MODEL_ID: &str = "gpt-5.5";

pub(crate) fn static_model_catalog() -> ModelsResponse {
    let catalog = ModelsResponse {
        models: vec![
            bedrock_model(
                bundled_openai_model(GPT_6_1_SOL_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID,
                "GPT-6.1 Sol",
                /*priority*/ 0,
            ),
            bedrock_model(
                bundled_openai_model(GPT_6_ASTRA_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID,
                "GPT-6-Astra",
                /*priority*/ 1,
            ),
            bedrock_model(
                bundled_openai_model(GPT_6_SOL_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID,
                "GPT-6 Sol",
                /*priority*/ 2,
            ),
            bedrock_model(
                bundled_openai_model(GPT_6_LUNA_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID,
                "GPT-6 Luna",
                /*priority*/ 3,
            ),
            bedrock_model(
                bundled_openai_model(GPT_5_6_SOL_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
                "GPT-5.6 Sol",
                /*priority*/ 4,
            ),
            bedrock_model(
                bundled_openai_model(GPT_5_6_TERRA_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID,
                "GPT-5.6 Terra",
                /*priority*/ 5,
            ),
            bedrock_model(
                bundled_openai_model(GPT_5_6_LUNA_OPENAI_MODEL_ID),
                AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
                "GPT-5.6 Luna",
                /*priority*/ 6,
            ),
            gpt_5_bedrock_model(
                GPT_5_5_OPENAI_MODEL_ID,
                AMAZON_BEDROCK_GPT_5_5_MODEL_ID,
                "GPT-5.5",
                /*priority*/ 7,
            ),
        ],
    };
    normalize_bundled_bedrock_catalog(catalog, BedrockEndpoint::Mantle)
}

pub(super) fn static_gov_model_catalog() -> ModelsResponse {
    let mut catalog = static_model_catalog();
    catalog.models.retain(|model| {
        matches!(
            model.slug.as_str(),
            AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID | AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID
        )
    });
    catalog
}

pub(super) fn normalize_bundled_bedrock_catalog(
    mut catalog: ModelsResponse,
    endpoint: BedrockEndpoint,
) -> ModelsResponse {
    for model in &mut catalog.models {
        // Bundled defaults must not inherit OpenAI-only tiers or opt into a premium tier.
        model.additional_speed_tiers.clear();
        model.service_tiers.clear();
        if matches!(
            (endpoint, model.slug.as_str()),
            (
                BedrockEndpoint::Mantle,
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID | AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID
            ) | (
                BedrockEndpoint::Runtime,
                "global.openai.gpt-6-astra"
                    | "us.openai.gpt-6-astra"
                    | "global.openai.gpt-6.1-sol"
                    | "us.openai.gpt-6.1-sol"
            )
        ) {
            model.service_tiers.push(ModelServiceTier {
                id: "ultrafast".to_string(),
                name: "Ultrafast".to_string(),
                description: "The fastest available responses for latency-sensitive work."
                    .to_string(),
            });
        }
        model.default_service_tier = None;
    }
    normalize_bedrock_catalog(catalog)
}

pub(super) fn normalize_bedrock_catalog(mut catalog: ModelsResponse) -> ModelsResponse {
    // Custom catalogs own their tier definitions; only normalize wire compatibility here.
    for model in &mut catalog.models {
        // Bedrock rejects the `search_content_types` field used by multimodal search.
        model.web_search_tool_type = WebSearchToolType::Text;
    }
    catalog
}

fn gpt_5_bedrock_model(
    openai_slug: &str,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    let mut model = bundled_openai_model(openai_slug);
    model.slug = bedrock_slug.to_string();
    model.display_name = display_name.to_string();
    model.priority = priority;
    model.context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model.max_context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model.visibility = ModelVisibility::List;
    model.availability_nux = None;
    model.upgrade = None;
    model
}

fn bedrock_model(
    mut model: ModelInfo,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    model.slug = bedrock_slug.to_string();
    model.display_name = display_name.to_string();
    model.priority = priority;
    model.visibility = ModelVisibility::List;
    model.availability_nux = None;
    model.upgrade = None;
    model.use_responses_lite = false;
    model
}

fn bundled_openai_model(slug: &str) -> ModelInfo {
    bundled_models_response()
        .unwrap_or_else(|err| panic!("bundled models.json should parse: {err}"))
        .models
        .into_iter()
        .find(|model| model.slug == slug)
        .unwrap_or_else(|| panic!("bundled models.json should include {slug}"))
}

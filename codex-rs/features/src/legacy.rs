use crate::Feature;
use tracing::info;

#[derive(Clone, Copy)]
struct Alias {
    legacy_key: &'static str,
    feature: Feature,
}

const ALIASES: &[Alias] = &[
    Alias {
        legacy_key: "connectors",
        feature: Feature::Apps,
    },
    Alias {
        legacy_key: "web_search",
        feature: Feature::WebSearchRequest,
    },
    Alias {
        legacy_key: "imagegenext",
        feature: Feature::ImageGeneration,
    },
    Alias {
        legacy_key: "collab",
        feature: Feature::Collab,
    },
    Alias {
        legacy_key: "memory_tool",
        feature: Feature::MemoryTool,
    },
    Alias {
        legacy_key: "codex_hooks",
        feature: Feature::CodexHooks,
    },
];

pub fn legacy_feature_keys() -> impl Iterator<Item = &'static str> {
    ALIASES.iter().map(|alias| alias.legacy_key)
}

pub(crate) fn feature_for_key(key: &str) -> Option<Feature> {
    ALIASES
        .iter()
        .find(|alias| alias.legacy_key == key)
        .map(|alias| {
            log_alias(alias.legacy_key, alias.feature);
            alias.feature
        })
}

fn log_alias(alias: &str, feature: Feature) {
    let canonical = feature.key();
    if alias == canonical {
        return;
    }
    info!(
        %alias,
        canonical,
        "legacy feature toggle detected; prefer `[features].{canonical}`"
    );
}

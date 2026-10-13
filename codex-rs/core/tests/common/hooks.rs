use codex_core::config::Config;
use codex_features::Feature;

/// Enables hooks and checks the fixture actually configured at least one; hooks run as soon as
/// they are discovered.
pub fn enable_discovered_hooks(config: &mut Config) {
    config.features.enable(Feature::CodexHooks);

    let listed = codex_hooks::list_hooks(codex_hooks::HooksConfig {
        feature_enabled: true,
        config_layer_stack: Some(config.config_layer_stack.clone()),
        ..codex_hooks::HooksConfig::default()
    });
    assert!(
        !listed.hooks.is_empty(),
        "hook fixture should discover at least one hook"
    );
}

use std::collections::HashMap;

use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStack;
use codex_config::HookStateToml;
use codex_config::TomlValue;

/// Build effective hook state from config layers that are allowed to override
/// user preferences.
///
/// This intentionally reads only user and session flag layers, including
/// disabled layers, to match the skills config behavior. Project and managed
/// layers can discover hooks, but they do not get to write user hook state.
pub fn hook_states_from_stack(
    config_layer_stack: Option<&ConfigLayerStack>,
) -> HashMap<String, HookStateToml> {
    let Some(config_layer_stack) = config_layer_stack else {
        return HashMap::new();
    };

    let mut states: HashMap<String, HookStateToml> = HashMap::new();
    for layer in config_layer_stack.all_layers_low_to_high() {
        if !matches!(
            layer.name,
            ConfigLayerSource::User { .. } | ConfigLayerSource::SessionFlags
        ) {
            continue;
        }

        let Some(state_value) = layer
            .config
            .get("hooks")
            .and_then(|hooks| hooks.get("state"))
        else {
            continue;
        };
        let TomlValue::Table(state_by_key) = state_value else {
            continue;
        };

        for (key, state_value) in state_by_key {
            let state: HookStateToml = match state_value.clone().try_into() {
                Ok(state) => state,
                Err(_) => {
                    continue;
                }
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            // Later layers win field-by-field so a future per-hook state write
            // does not accidentally erase an existing enablement override.
            let effective_state = states.entry(key.to_string()).or_default();
            if let Some(enabled) = state.enabled {
                effective_state.enabled = Some(enabled);
            }
        }
    }

    states
}

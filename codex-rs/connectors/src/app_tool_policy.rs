use codex_config::ConfigLayerStack;
use codex_config::types::AppsConfigToml;
use serde::Deserialize;

use crate::AppInfo;

/// App and account metadata used to evaluate one tool.
#[derive(Debug, Clone, Copy)]
pub struct AppToolPolicyInput<'a> {
    pub connector_id: Option<&'a str>,
    pub tool_name: &'a str,
    pub tool_title: Option<&'a str>,
    pub destructive_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

/// Resolves app tool enablement against one immutable config snapshot.
///
/// Callers should construct one evaluator and reuse it for every tool in the
/// same exposure build so config layers are merged and decoded only once.
pub struct AppToolPolicyEvaluator {
    apps_config: Option<AppsConfigToml>,
}

impl AppToolPolicyEvaluator {
    pub fn new(config_layer_stack: &ConfigLayerStack) -> Self {
        Self::from_apps_config(apps_config_from_layer_stack(config_layer_stack))
    }

    pub fn from_apps_config(apps_config: Option<AppsConfigToml>) -> Self {
        Self { apps_config }
    }

    /// Returns whether config exposes this app tool to the model.
    pub fn tool_enabled(&self, input: AppToolPolicyInput<'_>) -> bool {
        self.apps_config
            .as_ref()
            .is_none_or(|apps_config| tool_enabled_in_apps_config(apps_config, input))
    }

    /// Returns the effective enablement for one connector.
    pub fn app_enabled(&self, connector_id: &str) -> bool {
        self.apps_config
            .as_ref()
            .map(|apps_config| app_is_enabled(apps_config, Some(connector_id)))
            .unwrap_or(true)
    }

    /// Applies app policy without overriding source state for unconfigured apps.
    pub fn apply_app_enabled_state(&self, mut apps: Vec<AppInfo>) -> Vec<AppInfo> {
        let Some(apps_config) = self.apps_config.as_ref() else {
            return apps;
        };

        for app in &mut apps {
            if apps_config.default.is_some() || apps_config.apps.contains_key(app.id.as_str()) {
                app.is_enabled = self.app_enabled(app.id.as_str());
            }
        }

        apps
    }
}

/// Reads the merged, unmanaged Apps configuration from a config-layer stack.
pub fn apps_config_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
) -> Option<AppsConfigToml> {
    config_layer_stack
        .effective_config()
        .as_table()
        .and_then(|table| table.get("apps"))
        .cloned()
        .and_then(|value| AppsConfigToml::deserialize(value).ok())
}

pub fn app_is_enabled(apps_config: &AppsConfigToml, connector_id: Option<&str>) -> bool {
    let default_enabled = apps_config
        .default
        .as_ref()
        .map(|defaults| defaults.enabled)
        .unwrap_or(true);

    connector_id
        .and_then(|connector_id| apps_config.apps.get(connector_id))
        .map(|app| app.enabled)
        .unwrap_or(default_enabled)
}

fn tool_enabled_in_apps_config(
    apps_config: &AppsConfigToml,
    input: AppToolPolicyInput<'_>,
) -> bool {
    let app = input
        .connector_id
        .and_then(|connector_id| apps_config.apps.get(connector_id));
    let tools = app.and_then(|app| app.tools.as_ref());
    let tool_config = tools.and_then(|tools| {
        tools
            .tools
            .get(input.tool_name)
            .or_else(|| input.tool_title.and_then(|title| tools.tools.get(title)))
    });

    if !app_is_enabled(apps_config, input.connector_id) {
        return false;
    }

    if let Some(enabled) = tool_config
        .and_then(|tool| tool.enabled)
        .or_else(|| app.and_then(|app| app.default_tools_enabled))
    {
        return enabled;
    }

    let app_defaults = apps_config.default.as_ref();
    let destructive_enabled = app
        .and_then(|app| app.destructive_enabled)
        .unwrap_or_else(|| {
            app_defaults
                .map(|defaults| defaults.destructive_enabled)
                .unwrap_or(true)
        });
    let open_world_enabled = app
        .and_then(|app| app.open_world_enabled)
        .unwrap_or_else(|| {
            app_defaults
                .map(|defaults| defaults.open_world_enabled)
                .unwrap_or(true)
        });
    let destructive_hint = input.destructive_hint.unwrap_or(true);
    let open_world_hint = input.open_world_hint.unwrap_or(true);
    (destructive_enabled || !destructive_hint) && (open_world_enabled || !open_world_hint)
}

#[cfg(test)]
#[path = "app_tool_policy_tests.rs"]
mod tests;

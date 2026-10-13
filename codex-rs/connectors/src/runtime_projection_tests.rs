use codex_config::AbsolutePathBuf;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerStack;
use codex_config::TomlValue;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn projection_deduplicates_apps_and_ignores_non_runtime_tools() {
    let config = ConfigLayerStack::new(Vec::new()).expect("config layer stack");
    let apps = installed_connector_runtime(
        &config,
        [
            tool(Some(" drive "), /*connector_name*/ None, "files/list"),
            tool(Some("drive"), Some(" Drive "), "files/get"),
            ConnectorRuntimeTool {
                synthetic: true,
                ..tool(Some("synthetic"), Some("Synthetic"), "link")
            },
            tool(Some(" "), Some("Empty"), "empty"),
            tool(/*connector_id*/ None, Some("Missing"), "missing"),
        ],
    );

    assert_eq!(
        apps,
        vec![InstalledConnectorRuntime {
            id: "drive".to_string(),
            runtime_name: Some("Drive".to_string()),
            enabled: true,
            callable: true,
        }]
    );
}

#[test]
fn projection_applies_app_config_and_model_visibility() {
    let mut disabled_app = TomlValue::Table(Default::default());
    disabled_app
        .as_table_mut()
        .expect("app table")
        .insert("enabled".to_string(), TomlValue::Boolean(false));
    let mut apps = TomlValue::Table(Default::default());
    apps.as_table_mut()
        .expect("apps table")
        .insert("disabled".to_string(), disabled_app);
    let mut user_config = TomlValue::Table(Default::default());
    user_config
        .as_table_mut()
        .expect("user config table")
        .insert("apps".to_string(), apps);
    let config_toml_path = AbsolutePathBuf::try_from(std::env::temp_dir().join(CONFIG_TOML_FILE))
        .expect("absolute config path");
    let config = ConfigLayerStack::new(Vec::new())
        .expect("config layer stack")
        .with_user_config(&config_toml_path, user_config)
        .expect("apps user config should be valid");
    let apps = installed_connector_runtime(
        &config,
        [
            tool(Some("disabled"), Some("Disabled"), "disabled/tool"),
            ConnectorRuntimeTool {
                model_visible: false,
                ..tool(Some("hidden"), Some("Hidden"), "hidden/tool")
            },
            tool(Some("callable"), Some("Callable"), "callable/tool"),
        ],
    );

    assert_eq!(
        apps,
        vec![
            InstalledConnectorRuntime {
                id: "callable".to_string(),
                runtime_name: Some("Callable".to_string()),
                enabled: true,
                callable: true,
            },
            InstalledConnectorRuntime {
                id: "disabled".to_string(),
                runtime_name: Some("Disabled".to_string()),
                enabled: false,
                callable: false,
            },
            InstalledConnectorRuntime {
                id: "hidden".to_string(),
                runtime_name: Some("Hidden".to_string()),
                enabled: true,
                callable: false,
            },
        ]
    );
}

fn tool<'a>(
    connector_id: Option<&'a str>,
    connector_name: Option<&'a str>,
    tool_name: &'a str,
) -> ConnectorRuntimeTool<'a> {
    ConnectorRuntimeTool {
        connector_id,
        connector_name,
        tool_name,
        tool_title: None,
        destructive_hint: None,
        open_world_hint: None,
        synthetic: false,
        model_visible: true,
    }
}

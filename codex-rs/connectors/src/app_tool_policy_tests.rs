use std::collections::HashMap;

use codex_config::AbsolutePathBuf;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerStack;
use codex_config::TomlValue;
use codex_config::types::AppConfig;
use codex_config::types::AppToolConfig;
use codex_config::types::AppToolsConfig;
use codex_config::types::AppsConfigToml;
use codex_config::types::AppsDefaultConfig;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn evaluator_reuses_one_snapshot_across_tools() {
    let apps_config = AppsConfigToml {
        default: None,
        apps: HashMap::from([(
            "calendar".to_string(),
            AppConfig {
                enabled: true,
                default_tools_enabled: Some(false),
                tools: Some(AppToolsConfig {
                    tools: HashMap::from([(
                        "events/create".to_string(),
                        AppToolConfig {
                            enabled: Some(true),
                        },
                    )]),
                }),
                ..Default::default()
            },
        )]),
    };
    let evaluator = evaluator_from_layer_stack(Some(&apps_config));

    assert_eq!(
        [
            evaluator.tool_enabled(input("events/create", /*tool_title*/ None)),
            evaluator.tool_enabled(input("events/list", /*tool_title*/ None)),
            evaluator.tool_enabled(input("calendar_events/create", Some("events/create"))),
        ],
        [true, false, true]
    );
}

#[test]
fn evaluator_without_apps_config_enables_everything() {
    let evaluator = evaluator_from_layer_stack(/*apps_config*/ None);

    assert!(evaluator.tool_enabled(input("events/create", /*tool_title*/ None)));
    assert!(evaluator.app_enabled("calendar"));
}

#[test]
fn evaluator_uses_global_defaults_for_destructive_hints() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ true, /*destructive_enabled*/ false,
            /*open_world_enabled*/ true,
        )),
        apps: HashMap::new(),
    };

    assert!(!tool_enabled(
        apps_config,
        Some("calendar"),
        "events/create",
        Some(true),
        /*open_world_hint*/ None,
    ));
}

#[test]
fn evaluator_defaults_missing_destructive_hint_to_true() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ true, /*destructive_enabled*/ false,
            /*open_world_enabled*/ true,
        )),
        apps: HashMap::new(),
    };

    assert!(!tool_enabled(
        apps_config,
        Some("calendar"),
        "events/create",
        /*destructive_hint*/ None,
        Some(false),
    ));
}

#[test]
fn evaluator_defaults_missing_open_world_hint_to_true() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ true, /*destructive_enabled*/ true,
            /*open_world_enabled*/ false,
        )),
        apps: HashMap::new(),
    };

    assert!(!tool_enabled(
        apps_config,
        Some("calendar"),
        "events/create",
        Some(false),
        /*open_world_hint*/ None,
    ));
}

#[test]
fn app_enablement_uses_defaults_and_per_app_overrides() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ false, /*destructive_enabled*/ true,
            /*open_world_enabled*/ true,
        )),
        apps: HashMap::from([(
            "calendar".to_string(),
            AppConfig {
                enabled: true,
                ..Default::default()
            },
        )]),
    };

    assert_eq!(
        [
            app_is_enabled(&apps_config, Some("calendar")),
            app_is_enabled(&apps_config, Some("drive")),
            app_is_enabled(&apps_config, /*connector_id*/ None),
        ],
        [true, false, false]
    );

    let evaluator = AppToolPolicyEvaluator::from_apps_config(Some(apps_config));
    assert_eq!(
        evaluator.apply_app_enabled_state(vec![
            app("calendar", /*enabled*/ false),
            app("drive", /*enabled*/ true),
        ]),
        vec![
            app("calendar", /*enabled*/ true),
            app("drive", /*enabled*/ false),
        ]
    );
}

#[test]
fn app_enablement_preserves_source_state_for_unconfigured_apps() {
    let apps_config = AppsConfigToml {
        default: None,
        apps: HashMap::from([
            (
                "calendar".to_string(),
                AppConfig {
                    enabled: true,
                    ..Default::default()
                },
            ),
            (
                "drive".to_string(),
                AppConfig {
                    enabled: false,
                    ..Default::default()
                },
            ),
        ]),
    };
    let evaluator = AppToolPolicyEvaluator::from_apps_config(Some(apps_config));

    assert_eq!(
        evaluator.apply_app_enabled_state(vec![
            app("calendar", /*enabled*/ false),
            app("drive", /*enabled*/ true),
            app("slack", /*enabled*/ false),
            app("gmail", /*enabled*/ true),
        ]),
        vec![
            app("calendar", /*enabled*/ true),
            app("drive", /*enabled*/ false),
            app("slack", /*enabled*/ false),
            app("gmail", /*enabled*/ true),
        ]
    );
}

#[test]
fn evaluator_honors_default_app_enabled_false() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ false, /*destructive_enabled*/ true,
            /*open_world_enabled*/ true,
        )),
        apps: HashMap::new(),
    };

    assert!(!tool_enabled(
        apps_config,
        Some("calendar"),
        "events/list",
        /*destructive_hint*/ None,
        /*open_world_hint*/ None,
    ));
}

#[test]
fn evaluator_allows_per_app_enable_when_default_is_disabled() {
    let apps_config = AppsConfigToml {
        default: Some(defaults(
            /*enabled*/ false, /*destructive_enabled*/ true,
            /*open_world_enabled*/ true,
        )),
        apps: HashMap::from([(
            "calendar".to_string(),
            AppConfig {
                enabled: true,
                ..Default::default()
            },
        )]),
    };

    assert!(tool_enabled(
        apps_config,
        Some("calendar"),
        "events/list",
        /*destructive_hint*/ None,
        /*open_world_hint*/ None,
    ));
}

#[test]
fn per_tool_enable_overrides_app_level_hints() {
    let apps_config = AppsConfigToml {
        default: None,
        apps: HashMap::from([(
            "calendar".to_string(),
            AppConfig {
                enabled: true,
                destructive_enabled: Some(false),
                open_world_enabled: Some(false),
                tools: Some(AppToolsConfig {
                    tools: HashMap::from([(
                        "events/create".to_string(),
                        AppToolConfig {
                            enabled: Some(true),
                        },
                    )]),
                }),
                ..Default::default()
            },
        )]),
    };

    assert!(tool_enabled(
        apps_config,
        Some("calendar"),
        "events/create",
        Some(true),
        Some(true),
    ));
}

#[test]
fn default_tools_enable_overrides_app_level_hints() {
    let mut app = AppConfig {
        enabled: true,
        destructive_enabled: Some(false),
        open_world_enabled: Some(false),
        default_tools_enabled: Some(true),
        ..Default::default()
    };
    let apps_config = |app: AppConfig| AppsConfigToml {
        default: None,
        apps: HashMap::from([("calendar".to_string(), app)]),
    };

    let enabled = tool_enabled(
        apps_config(app.clone()),
        Some("calendar"),
        "events/create",
        Some(true),
        Some(true),
    );
    app.destructive_enabled = Some(true);
    app.open_world_enabled = Some(true);
    app.default_tools_enabled = Some(false);
    let disabled = tool_enabled(
        apps_config(app),
        Some("calendar"),
        "events/list",
        /*destructive_hint*/ None,
        /*open_world_hint*/ None,
    );

    assert_eq!([enabled, disabled], [true, false]);
}

fn input<'a>(tool_name: &'a str, tool_title: Option<&'a str>) -> AppToolPolicyInput<'a> {
    AppToolPolicyInput {
        connector_id: Some("calendar"),
        tool_name,
        tool_title,
        destructive_hint: Some(true),
        open_world_hint: Some(true),
    }
}

fn app(id: &str, enabled: bool) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: id.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: enabled,
        plugin_display_names: Vec::new(),
    }
}

fn tool_enabled(
    apps_config: AppsConfigToml,
    connector_id: Option<&str>,
    tool_name: &str,
    destructive_hint: Option<bool>,
    open_world_hint: Option<bool>,
) -> bool {
    AppToolPolicyEvaluator::from_apps_config(Some(apps_config)).tool_enabled(AppToolPolicyInput {
        connector_id,
        tool_name,
        tool_title: None,
        destructive_hint,
        open_world_hint,
    })
}

fn evaluator_from_layer_stack(apps_config: Option<&AppsConfigToml>) -> AppToolPolicyEvaluator {
    let config_layer_stack = ConfigLayerStack::new(Vec::new()).expect("config layer stack");
    let config_layer_stack = if let Some(apps_config) = apps_config {
        let mut user_config = TomlValue::Table(Default::default());
        user_config
            .as_table_mut()
            .expect("user config table")
            .insert(
                "apps".to_string(),
                TomlValue::try_from(apps_config).expect("serialize apps config"),
            );
        let config_toml_path =
            AbsolutePathBuf::try_from(std::env::temp_dir().join(CONFIG_TOML_FILE))
                .expect("absolute config path");
        config_layer_stack
            .with_user_config(&config_toml_path, user_config)
            .expect("apps user config should be valid")
    } else {
        config_layer_stack
    };
    AppToolPolicyEvaluator::new(&config_layer_stack)
}

fn defaults(
    enabled: bool,
    destructive_enabled: bool,
    open_world_enabled: bool,
) -> AppsDefaultConfig {
    AppsDefaultConfig {
        enabled,
        destructive_enabled,
        open_world_enabled,
    }
}

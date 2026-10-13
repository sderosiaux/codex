use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigError;
use codex_config::ConfigLayerEntry;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLoadError;
use codex_config::ConfigLoadOptions;
use codex_config::LoaderOverrides;
use codex_config::SessionThreadConfig;
use codex_config::StaticThreadConfigLoader;
use codex_config::ThreadConfigSource;
use codex_config::config_error_from_ignored_toml_fields;
use codex_config::config_error_from_toml;
use codex_config::config_error_from_typed_toml;
use codex_config::config_toml::ConfigToml;
use codex_config::loader::load_config_layers_state;
use codex_exec_server::LOCAL_FS;
use codex_protocol::config_types::EnvironmentVariablePattern;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::path::Path;
use tempfile::tempdir;
use toml::Value as TomlValue;

fn config_error_from_io(err: &std::io::Error) -> &ConfigError {
    err.get_ref()
        .and_then(|err| err.downcast_ref::<ConfigLoadError>())
        .map(ConfigLoadError::config_error)
        .expect("expected ConfigLoadError")
}

async fn make_config_for_test(
    codex_home: &Path,
    project_root_markers: Option<Vec<String>>,
) -> std::io::Result<()> {
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        toml::to_string(&ConfigToml {
            project_root_markers,
            ..Default::default()
        })
        .expect("serialize config"),
    )
    .await
}

async fn write_linked_worktree_pointer(
    repo_root: &Path,
    worktree_root: &Path,
) -> std::io::Result<()> {
    let worktree_git_dir = repo_root.join(".git/worktrees/feature-x");
    tokio::fs::create_dir_all(&worktree_git_dir).await?;
    tokio::fs::write(
        worktree_root.join(".git"),
        format!("gitdir: {}\n", worktree_git_dir.display()),
    )
    .await?;
    tokio::fs::write(
        worktree_git_dir.join("gitdir"),
        format!("{}\n", worktree_root.join(".git").display()),
    )
    .await?;
    tokio::fs::write(worktree_git_dir.join("commondir"), "../..\n").await
}

async fn write_project_hook_config(
    dot_codex_folder: &Path,
    foo: Option<&str>,
    command: &str,
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dot_codex_folder).await?;
    let foo = foo
        .map(|value| format!("foo = \"{value}\"\n\n"))
        .unwrap_or_default();
    tokio::fs::write(
        dot_codex_folder.join(CONFIG_TOML_FILE),
        format!(
            r#"{foo}[hooks]

[[hooks.PreToolUse]]
matcher = "Bash"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "{command}"
"#
        ),
    )
    .await
}

#[tokio::test]
async fn cli_overrides_resolve_relative_paths_against_cwd() -> std::io::Result<()> {
    let codex_home = tempdir().expect("tempdir");
    let cwd_dir = tempdir().expect("tempdir");
    let cwd_path = cwd_dir.path().to_path_buf();

    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .cli_overrides(vec![(
            "log_dir".to_string(),
            TomlValue::String("run-logs".to_string()),
        )])
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd_path.clone()),
            ..Default::default()
        })
        .build()
        .await?;

    let expected = AbsolutePathBuf::resolve_path_against_base("run-logs", cwd_path);
    assert_eq!(config.log_dir, expected.to_path_buf());
    Ok(())
}

#[tokio::test]
async fn returns_config_error_for_invalid_user_config_toml() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"model = "gpt-4"
invalid = ["#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let cwd = AbsolutePathBuf::try_from(tmp.path()).expect("cwd");
    let err = load_config_layers_state(
        LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await
    .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    let expected_toml_error = toml::from_str::<TomlValue>(contents).expect_err("parse error");
    let expected_config_error = config_error_from_toml(&config_path, contents, expected_toml_error);
    assert_eq!(config_error, &expected_config_error);
}

#[tokio::test]
async fn ignore_user_config_keeps_empty_user_layer() -> std::io::Result<()> {
    let tmp = tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"model = "from-user-config"
invalid = ["#,
    )
    .expect("write config");

    let cwd = AbsolutePathBuf::try_from(tmp.path()).expect("cwd");
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides {
            ignore_user_config: true,
            ..Default::default()
        },
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let user_layer = layers
        .get_active_user_layer()
        .expect("expected a user layer even when CODEX_HOME/config.toml is ignored");
    assert_eq!(
        user_layer.config,
        TomlValue::Table(toml::map::Map::new()),
        "expected ignored user config to preserve only layer metadata"
    );
    assert_eq!(layers.effective_config().get("model"), None);
    Ok(())
}

#[tokio::test]
async fn ignore_project_config_skips_project_discovery() -> std::io::Result<()> {
    let tmp = tempdir().expect("tempdir");
    let codex_home = tmp.path().join("home");
    let workspace = tmp.path().join("workspace");
    tokio::fs::create_dir_all(codex_home.as_path()).await?;
    tokio::fs::create_dir_all(workspace.join(".git")).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;
    let project_config_dir = workspace.join(".codex");
    tokio::fs::create_dir_all(&project_config_dir).await?;
    tokio::fs::write(
        project_config_dir.join(CONFIG_TOML_FILE),
        r#"model = "from-project"
invalid = ["#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&workspace)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[(
            "model".to_string(),
            TomlValue::String("from-session".to_string()),
        )],
        ConfigLoadOptions {
            loader_overrides: LoaderOverrides {
                ignore_project_config: true,
                ..LoaderOverrides::without_managed_config_for_tests()
            },
            ..Default::default()
        },
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    assert!(
        layers
            .layers_low_to_high()
            .all(|layer| !matches!(layer.name, ConfigLayerSource::Project { .. }))
    );
    assert_eq!(
        layers.effective_config().get("model"),
        Some(&TomlValue::String("from-session".to_string()))
    );
    Ok(())
}

#[tokio::test]
async fn returns_config_error_for_schema_error_in_user_config() {
    let tmp = tempdir().expect("tempdir");
    let contents = "model_context_window = \"not_a_number\"";
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    let _guard = codex_utils_absolute_path::AbsolutePathBufGuard::new(tmp.path());
    let expected_config_error =
        codex_config::config_error_from_typed_toml::<ConfigToml>(&config_path, contents)
            .expect("schema error");
    assert_eq!(config_error, &expected_config_error);
}

#[tokio::test]
async fn strict_config_rejects_unknown_user_config_key() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"model = "gpt-5"
unknown_key = true"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    let expected_config_error =
        config_error_from_ignored_toml_fields::<ConfigToml>(&config_path, contents)
            .expect("unknown field error");
    assert_eq!(config_error, &expected_config_error);
}

#[tokio::test]
async fn non_strict_config_rejects_mixed_shell_environment_policy_before_higher_layer_merge() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy]
exclude = ["LEGACY_*"]

[shell_environment_policy.filters]
"CANONICAL_*" = "include"
"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "shell_environment_policy.filters.HIGHER_*".to_string(),
            TomlValue::String("exclude".to_string()),
        )])
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect_err("one config layer must not mix legacy lists and filters");

    assert_eq!(
        config_error_from_io(&err),
        &config_error_from_typed_toml::<ConfigToml>(&config_path, contents)
            .expect("mixed shell policy should produce a typed config error")
    );
}

#[tokio::test]
async fn shell_environment_policy_unknown_fields_follow_strict_config() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy]
future_field = true
"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect("non-strict config should ignore unknown fields");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("strict config should reject unknown fields");

    assert_eq!(
        config_error_from_io(&err).message,
        "unknown configuration field `shell_environment_policy.future_field`"
    );
}

#[tokio::test]
async fn non_strict_config_merges_shell_filter_case_variants_across_layers() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"
[shell_environment_policy.filters]
"SECRET_TOKEN" = "exclude"
"#;
    std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

    let config = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "shell_environment_policy.filters.secret_token".to_string(),
            TomlValue::String("include".to_string()),
        )])
        .strict_config(/*strict_config*/ false)
        .build()
        .await
        .expect("higher-priority case-variant filter should override the lower layer");

    assert!(config.shell_environment_policy.exclude.is_empty());
    assert_eq!(
        config.shell_environment_policy.include_only,
        vec![EnvironmentVariablePattern::new_case_insensitive(
            "secret_token"
        )]
    );
}

#[tokio::test]
async fn non_strict_config_rejects_malformed_shell_policy_before_representation_conversion() {
    let cases = [
        (
            r#"
[shell_environment_policy]
exclude = ["SECRET_*", 17]
"#,
            vec![(
                "shell_environment_policy.filters.PATH".to_string(),
                TomlValue::String("include".to_string()),
            )],
        ),
        (
            r#"
[shell_environment_policy.filters]
"SECRET_*" = "keep"
"#,
            vec![(
                "shell_environment_policy.exclude".to_string(),
                TomlValue::Array(vec![TomlValue::String("PATH".to_string())]),
            )],
        ),
    ];

    for (contents, cli_overrides) in cases {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

        ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cli_overrides(cli_overrides)
            .strict_config(/*strict_config*/ false)
            .build()
            .await
            .expect_err("malformed shell policy should be rejected before conversion");
    }
}

#[tokio::test]
async fn non_strict_config_allows_replaced_shell_policy_fields_outside_filter_representation() {
    let cases = [
        (
            r#"
[shell_environment_policy]
inherit = "invalid"
set = ["invalid"]
"#,
            vec![
                (
                    "shell_environment_policy.inherit".to_string(),
                    TomlValue::String("core".to_string()),
                ),
                (
                    "shell_environment_policy.set.PATH".to_string(),
                    TomlValue::String("/bin".to_string()),
                ),
            ],
        ),
        (
            r#"shell_environment_policy = 17"#,
            vec![(
                "shell_environment_policy.inherit".to_string(),
                TomlValue::String("core".to_string()),
            )],
        ),
    ];

    for (contents, cli_overrides) in cases {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(CONFIG_TOML_FILE), contents).expect("write config");

        ConfigBuilder::default()
            .codex_home(tmp.path().to_path_buf())
            .fallback_cwd(Some(tmp.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .cli_overrides(cli_overrides)
            .strict_config(/*strict_config*/ false)
            .build()
            .await
            .expect("replaced shell policy fields should preserve normal overlay behavior");
    }
}

#[tokio::test]
async fn strict_config_rejects_unknown_cli_override_key() {
    let tmp = tempdir().expect("tempdir");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![(
            "foo".to_string(),
            TomlValue::String("bar".to_string()),
        )])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_cli_override_key_with_relative_path_override() {
    let tmp = tempdir().expect("tempdir");
    let instructions_path = tmp.path().join("instructions.md");
    std::fs::write(&instructions_path, "instructions").expect("write instructions");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![
            (
                "model_instructions_file".to_string(),
                TomlValue::String("instructions.md".to_string()),
            ),
            ("foo".to_string(), TomlValue::String("bar".to_string())),
        ])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_feature_cli_override_key() {
    let tmp = tempdir().expect("tempdir");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .cli_overrides(vec![("features.foo".to_string(), TomlValue::Boolean(true))])
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    assert_eq!(
        err.to_string(),
        "unknown configuration field `features.foo` in -c/--config override"
    );
}

#[tokio::test]
async fn strict_config_rejects_unknown_feature_user_config_key() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"[features]
foo = true"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .strict_config(/*strict_config*/ true)
        .build()
        .await
        .expect_err("expected error");

    let config_error = config_error_from_io(&err);
    assert_eq!(
        config_error.message,
        "unknown configuration field `features.foo`"
    );
    assert_eq!(config_error.range.start.line, 2);
    assert_eq!(config_error.range.start.column, 1);
}

#[test]
fn strict_config_points_to_unknown_nested_key() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"[mcp_servers.local]
command = "echo"
unknown_key = true"#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let error = config_error_from_ignored_toml_fields::<ConfigToml>(&config_path, contents)
        .expect("unknown field error");

    assert_eq!(
        error.message,
        "unknown configuration field `mcp_servers.local.unknown_key`"
    );
    assert_eq!(error.range.start.line, 3);
    assert_eq!(error.range.start.column, 1);
}
#[test]
fn schema_error_points_to_feature_value() {
    let tmp = tempdir().expect("tempdir");
    let contents = r#"[features]
collaboration_modes = "true""#;
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let _guard = codex_utils_absolute_path::AbsolutePathBufGuard::new(tmp.path());
    let error = codex_config::config_error_from_typed_toml::<ConfigToml>(&config_path, contents)
        .expect("schema error");

    let value_line = contents.lines().nth(1).expect("value line");
    let value_column = value_line.find("\"true\"").expect("value") + 1;
    assert_eq!(error.range.start.line, 2);
    assert_eq!(error.range.start.column, value_column);
}

#[tokio::test]
async fn returns_packaged_defaults_when_other_layers_are_missing() {
    let tmp = tempdir().expect("tempdir");
    let overrides = LoaderOverrides::without_managed_config_for_tests();

    let cwd = AbsolutePathBuf::try_from(tmp.path()).expect("cwd");
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[] as &[(String, TomlValue)],
        overrides,
        &codex_config::NoopThreadConfigLoader,
    )
    .await
    .expect("load layers");
    let user_layer = layers
        .get_active_user_layer()
        .expect("expected a user layer even when CODEX_HOME/config.toml does not exist");
    let expected_user_layer = ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, tmp.path()),
            profile: None,
        },
        TomlValue::Table(toml::map::Map::new()),
    );
    assert_eq!(&expected_user_layer, user_layer);
    assert_eq!(
        user_layer.config,
        TomlValue::Table(toml::map::Map::new()),
        "expected empty config for user layer when config.toml does not exist"
    );

    let packaged_defaults = layers
        .layers_low_to_high()
        .find(|layer| matches!(layer.name, ConfigLayerSource::PackagedDefaults { .. }))
        .expect("packaged defaults layer should always be present");
    assert_eq!(layers.effective_config(), packaged_defaults.config);
    let num_system_layers = layers
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::System { .. }))
        .count();
    assert_eq!(
        num_system_layers, 1,
        "system layer should always be present"
    );
}

#[tokio::test]
async fn selected_user_config_file_layers_over_base_user_config() {
    let tmp = tempdir().expect("tempdir");
    let selected_config = tmp.path().join("work.config.toml");

    std::fs::write(
        tmp.path().join(CONFIG_TOML_FILE),
        r#"
model = "gpt-main"
review_model = "gpt-review"
"#,
    )
    .expect("write default user config");
    std::fs::write(&selected_config, r#"model = "gpt-work""#).expect("write selected user config");

    let mut overrides = LoaderOverrides::without_managed_config_for_tests();
    overrides.user_config_path =
        Some(AbsolutePathBuf::from_absolute_path(&selected_config).expect("selected config path"));
    overrides.user_config_profile = Some("work".parse().expect("profile-v2 name"));

    let cwd = AbsolutePathBuf::try_from(tmp.path()).expect("cwd");
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[] as &[(String, TomlValue)],
        overrides,
        &codex_config::NoopThreadConfigLoader,
    )
    .await
    .expect("load layers");

    let user_layers = layers
        .layers_low_to_high()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::User { .. }))
        .collect::<Vec<_>>();
    assert_eq!(user_layers.len(), 2);
    assert_eq!(
        user_layers[0].name,
        ConfigLayerSource::User {
            file: AbsolutePathBuf::from_absolute_path(tmp.path().join(CONFIG_TOML_FILE))
                .expect("base user config path"),
            profile: None,
        }
    );
    let user_layer = layers.get_active_user_layer().expect("selected user layer");
    assert_eq!(
        user_layer.name,
        ConfigLayerSource::User {
            file: AbsolutePathBuf::from_absolute_path(&selected_config)
                .expect("selected user config path"),
            profile: Some("work".to_string()),
        }
    );
    assert_eq!(
        layers
            .effective_config()
            .get("model")
            .and_then(TomlValue::as_str),
        Some("gpt-work")
    );
    assert_eq!(
        layers
            .effective_config()
            .get("review_model")
            .and_then(TomlValue::as_str),
        Some("gpt-review")
    );
}

#[tokio::test]
async fn includes_thread_config_layers_in_stack() -> anyhow::Result<()> {
    let tmp = tempdir()?;
    let cwd_dir = tmp.path().join("project");
    tokio::fs::create_dir_all(&cwd_dir).await?;
    let cwd = AbsolutePathBuf::from_absolute_path(&cwd_dir)?;
    let overrides = LoaderOverrides::without_managed_config_for_tests();
    let expected_system_config = AbsolutePathBuf::from_absolute_path(
        overrides
            .system_config_path
            .as_ref()
            .expect("test overrides should include a system config path"),
    )?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        tmp.path(),
        Some(cwd),
        &[("features.plugins".to_string(), TomlValue::Boolean(true))],
        overrides,
        &StaticThreadConfigLoader::new(vec![ThreadConfigSource::Session(SessionThreadConfig {
            features: BTreeMap::from([("plugins".to_string(), false)]),
            ..Default::default()
        })]),
    )
    .await?;

    let layer_sources = layers
        .layers_high_to_low()
        .map(|layer| layer.name.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        layer_sources,
        vec![
            ConfigLayerSource::SessionFlags,
            ConfigLayerSource::SessionFlags,
            ConfigLayerSource::User {
                file: AbsolutePathBuf::resolve_path_against_base(CONFIG_TOML_FILE, tmp.path()),
                profile: None,
            },
            ConfigLayerSource::System {
                file: expected_system_config,
            },
            ConfigLayerSource::PackagedDefaults {
                file: AbsolutePathBuf::from_absolute_path(std::env::current_exe()?)?,
            },
        ]
    );
    assert_eq!(
        layers
            .effective_config()
            .get("features")
            .and_then(TomlValue::as_table)
            .and_then(|features| features.get("plugins")),
        Some(&TomlValue::Boolean(false))
    );

    Ok(())
}

#[tokio::test]
async fn project_layers_prefer_closest_cwd() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    tokio::fs::create_dir_all(nested.join(".codex")).await?;
    tokio::fs::create_dir_all(project_root.join(".codex")).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;

    tokio::fs::write(
        project_root.join(".codex").join(CONFIG_TOML_FILE),
        r#"foo = "root"
"#,
    )
    .await?;
    tokio::fs::write(
        nested.join(".codex").join(CONFIG_TOML_FILE),
        r#"foo = "child"
"#,
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;
    let cwd = AbsolutePathBuf::from_absolute_path(&nested)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter_map(|layer| match &layer.name {
            ConfigLayerSource::Project { dot_codex_folder } => Some(dot_codex_folder),
            _ => None,
        })
        .collect();
    assert_eq!(project_layers.len(), 2);
    assert_eq!(project_layers[0].as_path(), nested.join(".codex").as_path());
    assert_eq!(
        project_layers[1].as_path(),
        project_root.join(".codex").as_path()
    );

    let config = layers.effective_config();
    let foo = config
        .get("foo")
        .and_then(TomlValue::as_str)
        .expect("foo entry");
    assert_eq!(foo, "child");
    Ok(())
}

#[tokio::test]
async fn linked_worktree_project_layers_keep_worktree_config_but_use_root_repo_hooks()
-> std::io::Result<()> {
    let tmp = tempdir()?;
    let repo_root = tmp.path().join("repo");
    let repo_child = repo_root.join("child");
    let worktree_root = tmp.path().join("worktree");
    let worktree_child = worktree_root.join("child");

    tokio::fs::create_dir_all(worktree_root.join(".codex")).await?;
    tokio::fs::create_dir_all(worktree_child.join(".codex")).await?;
    write_linked_worktree_pointer(&repo_root, &worktree_root).await?;
    write_project_hook_config(
        &repo_root.join(".codex"),
        Some("repo-root"),
        "echo repo root hook",
    )
    .await?;
    write_project_hook_config(
        &repo_child.join(".codex"),
        Some("repo-child"),
        "echo repo child hook",
    )
    .await?;
    write_project_hook_config(
        &worktree_root.join(".codex"),
        Some("worktree-root"),
        "echo worktree root hook",
    )
    .await?;
    write_project_hook_config(
        &worktree_child.join(".codex"),
        Some("worktree-child"),
        "echo worktree child hook",
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&worktree_child)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    assert_eq!(project_layers.len(), 2);
    assert_eq!(
        project_layers[0].hooks_config_folder(),
        Some(AbsolutePathBuf::from_absolute_path(
            repo_child.join(".codex")
        )?)
    );
    assert_eq!(
        project_layers[1].hooks_config_folder(),
        Some(AbsolutePathBuf::from_absolute_path(
            repo_root.join(".codex")
        )?)
    );
    assert_eq!(
        project_layers[0]
            .config
            .get("foo")
            .and_then(TomlValue::as_str),
        Some("worktree-child")
    );
    assert_eq!(
        project_hook_command(project_layers[0]),
        Some("echo repo child hook")
    );
    assert_eq!(
        project_layers[1]
            .config
            .get("foo")
            .and_then(TomlValue::as_str),
        Some("worktree-root")
    );
    assert_eq!(
        project_hook_command(project_layers[1]),
        Some("echo repo root hook")
    );

    Ok(())
}

#[tokio::test]
async fn linked_worktree_project_layers_use_root_repo_hooks_without_worktree_config_toml()
-> std::io::Result<()> {
    let tmp = tempdir()?;
    let repo_root = tmp.path().join("repo");
    let worktree_root = tmp.path().join("worktree");

    tokio::fs::create_dir_all(worktree_root.join(".codex")).await?;
    write_linked_worktree_pointer(&repo_root, &worktree_root).await?;
    write_project_hook_config(
        &repo_root.join(".codex"),
        /*foo*/ None,
        "echo repo root hook",
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&worktree_root)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    assert_eq!(project_layers.len(), 1);
    assert_eq!(
        project_layers[0].hooks_config_folder(),
        Some(AbsolutePathBuf::from_absolute_path(
            repo_root.join(".codex")
        )?)
    );
    assert_eq!(
        project_hook_command(project_layers[0]),
        Some("echo repo root hook")
    );

    Ok(())
}

#[tokio::test]
async fn nested_project_root_markers_do_not_redirect_regular_repo_hooks() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let repo_root = tmp.path().join("repo");
    let project_root = repo_root.join("project");
    let nested = project_root.join("child");

    tokio::fs::create_dir_all(repo_root.join(".git")).await?;
    tokio::fs::create_dir_all(&project_root).await?;
    tokio::fs::write(project_root.join(".hg"), "hg").await?;
    write_project_hook_config(
        &repo_root.join(".codex"),
        /*foo*/ None,
        "echo repo root hook",
    )
    .await?;
    write_project_hook_config(
        &project_root.join(".codex"),
        /*foo*/ None,
        "echo project root hook",
    )
    .await?;
    write_project_hook_config(
        &nested.join(".codex"),
        /*foo*/ None,
        "echo nested hook",
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, Some(vec![".hg".to_string()])).await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&nested)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    assert_eq!(project_layers.len(), 2);
    assert_eq!(
        project_layers[0].hooks_config_folder(),
        Some(AbsolutePathBuf::from_absolute_path(nested.join(".codex"))?)
    );
    assert_eq!(
        project_layers[1].hooks_config_folder(),
        Some(AbsolutePathBuf::from_absolute_path(
            project_root.join(".codex")
        )?)
    );
    assert_eq!(
        project_hook_command(project_layers[0]),
        Some("echo nested hook")
    );
    assert_eq!(
        project_hook_command(project_layers[1]),
        Some("echo project root hook")
    );

    Ok(())
}

fn project_hook_command(layer: &ConfigLayerEntry) -> Option<&str> {
    layer
        .config
        .get("hooks")?
        .get("PreToolUse")?
        .as_array()?
        .first()?
        .get("hooks")?
        .as_array()?
        .first()?
        .get("command")?
        .as_str()
}

#[tokio::test]
async fn project_paths_resolve_relative_to_dot_codex_and_override_in_order() -> std::io::Result<()>
{
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    tokio::fs::create_dir_all(project_root.join(".codex")).await?;
    tokio::fs::create_dir_all(nested.join(".codex")).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;

    let root_cfg = r#"
model_instructions_file = "root.txt"
"#;
    let nested_cfg = r#"
model_instructions_file = "child.txt"
"#;
    tokio::fs::write(project_root.join(".codex").join(CONFIG_TOML_FILE), root_cfg).await?;
    tokio::fs::write(nested.join(".codex").join(CONFIG_TOML_FILE), nested_cfg).await?;
    tokio::fs::write(
        project_root.join(".codex").join("root.txt"),
        "root instructions",
    )
    .await?;
    tokio::fs::write(
        nested.join(".codex").join("child.txt"),
        "child instructions",
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .harness_overrides(ConfigOverrides {
            cwd: Some(nested.clone()),
            ..ConfigOverrides::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("child instructions")
    );

    Ok(())
}

#[tokio::test]
async fn cli_override_model_instructions_file_sets_base_instructions() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(codex_home.join(CONFIG_TOML_FILE), "").await?;

    let cwd = tmp.path().join("work");
    tokio::fs::create_dir_all(&cwd).await?;

    let instructions_path = tmp.path().join("instr.md");
    tokio::fs::write(&instructions_path, "cli override instructions").await?;

    let cli_overrides = vec![(
        "model_instructions_file".to_string(),
        TomlValue::String(instructions_path.to_string_lossy().to_string()),
    )];

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(cli_overrides)
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd),
            ..ConfigOverrides::default()
        })
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("cli override instructions")
    );

    Ok(())
}

#[tokio::test]
async fn inline_instructions_set_base_instructions() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"instructions = "snapshot instructions""#,
    )
    .await?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home)
        .build()
        .await?;

    assert_eq!(
        config.base_instructions.as_deref(),
        Some("snapshot instructions")
    );

    Ok(())
}

#[tokio::test]
async fn project_layer_is_added_when_dot_codex_exists_without_config_toml() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    tokio::fs::create_dir_all(&nested).await?;
    tokio::fs::create_dir_all(project_root.join(".codex")).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;
    let cwd = AbsolutePathBuf::from_absolute_path(&nested)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    let expected_project_layer = ConfigLayerEntry::new(
        ConfigLayerSource::Project {
            dot_codex_folder: AbsolutePathBuf::from_absolute_path(project_root.join(".codex"))?,
        },
        TomlValue::Table(toml::map::Map::new()),
    );
    assert_eq!(vec![&expected_project_layer], project_layers);

    Ok(())
}

#[tokio::test]
async fn codex_home_is_not_loaded_as_project_layer_from_home_dir() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let home_dir = tmp.path().join("home");
    let codex_home = home_dir.join(".codex");
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"foo = "user"
"#,
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&home_dir)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .all_layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();
    let expected: Vec<&ConfigLayerEntry> = Vec::new();
    assert_eq!(expected, project_layers);
    assert_eq!(
        layers.effective_config().get("foo"),
        Some(&TomlValue::String("user".to_string()))
    );

    Ok(())
}

#[tokio::test]
async fn codex_home_within_project_tree_is_not_double_loaded() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    let project_dot_codex = project_root.join(".codex");
    let nested_dot_codex = nested.join(".codex");

    tokio::fs::create_dir_all(&nested_dot_codex).await?;
    tokio::fs::create_dir_all(project_root.join(".git")).await?;
    tokio::fs::write(project_root.join(".git/HEAD"), "ref: refs/heads/main\n").await?;
    tokio::fs::write(
        nested_dot_codex.join(CONFIG_TOML_FILE),
        r#"foo = "child"
"#,
    )
    .await?;

    tokio::fs::create_dir_all(&project_dot_codex).await?;
    make_config_for_test(&project_dot_codex, /*project_root_markers*/ None).await?;
    let user_config_path = project_dot_codex.join(CONFIG_TOML_FILE);
    let user_config_contents = tokio::fs::read_to_string(&user_config_path).await?;
    tokio::fs::write(
        &user_config_path,
        format!(
            r#"foo = "user"
{user_config_contents}"#
        ),
    )
    .await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&nested)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &project_dot_codex,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .all_layers_high_to_low()
        .filter(|layer| matches!(layer.name, ConfigLayerSource::Project { .. }))
        .collect();

    let child_config: TomlValue = toml::from_str(
        r#"foo = "child"
"#,
    )
    .expect("parse child config");
    let expected_project_layer = ConfigLayerEntry::new(
        ConfigLayerSource::Project {
            dot_codex_folder: AbsolutePathBuf::from_absolute_path(&nested_dot_codex)?,
        },
        child_config,
    );
    assert_eq!(vec![&expected_project_layer], project_layers);
    assert_eq!(
        layers.effective_config().get("foo"),
        Some(&TomlValue::String("child".to_string()))
    );

    Ok(())
}

#[tokio::test]
async fn cli_override_can_update_project_local_mcp_server() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    let dot_codex = project_root.join(".codex");
    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&nested).await?;
    tokio::fs::create_dir_all(&dot_codex).await?;
    tokio::fs::create_dir_all(&codex_home).await?;
    tokio::fs::write(project_root.join(".git"), "gitdir: here").await?;
    tokio::fs::write(
        dot_codex.join(CONFIG_TOML_FILE),
        r#"
[mcp_servers.sentry]
url = "https://mcp.sentry.dev/mcp"
enabled = false
"#,
    )
    .await?;
    make_config_for_test(&codex_home, /*project_root_markers*/ None).await?;

    let config = ConfigBuilder::default()
        .codex_home(codex_home)
        .cli_overrides(vec![(
            "mcp_servers.sentry.enabled".to_string(),
            TomlValue::Boolean(true),
        )])
        .fallback_cwd(Some(nested))
        .build()
        .await?;

    let server = config
        .mcp_servers
        .get("sentry")
        .expect("project MCP server should load");
    assert!(server.enabled);

    Ok(())
}

#[tokio::test]
async fn project_root_markers_supports_alternate_markers() -> std::io::Result<()> {
    let tmp = tempdir()?;
    let project_root = tmp.path().join("project");
    let nested = project_root.join("child");
    tokio::fs::create_dir_all(project_root.join(".codex")).await?;
    tokio::fs::create_dir_all(nested.join(".codex")).await?;
    tokio::fs::write(project_root.join(".hg"), "hg").await?;
    tokio::fs::write(
        project_root.join(".codex").join(CONFIG_TOML_FILE),
        r#"foo = "root"
"#,
    )
    .await?;
    tokio::fs::write(
        nested.join(".codex").join(CONFIG_TOML_FILE),
        r#"foo = "child"
"#,
    )
    .await?;

    let codex_home = tmp.path().join("home");
    tokio::fs::create_dir_all(&codex_home).await?;
    make_config_for_test(&codex_home, Some(vec![".hg".to_string()])).await?;

    let cwd = AbsolutePathBuf::from_absolute_path(&nested)?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        &codex_home,
        Some(cwd),
        &[] as &[(String, TomlValue)],
        LoaderOverrides::default(),
        &codex_config::NoopThreadConfigLoader,
    )
    .await?;

    let project_layers: Vec<_> = layers
        .layers_high_to_low()
        .filter_map(|layer| match &layer.name {
            ConfigLayerSource::Project { dot_codex_folder } => Some(dot_codex_folder),
            _ => None,
        })
        .collect();
    assert_eq!(project_layers.len(), 2);
    assert_eq!(project_layers[0].as_path(), nested.join(".codex").as_path());
    assert_eq!(
        project_layers[1].as_path(),
        project_root.join(".codex").as_path()
    );

    let merged = layers.effective_config();
    let foo = merged
        .get("foo")
        .and_then(TomlValue::as_str)
        .expect("foo entry");
    assert_eq!(foo, "child");

    Ok(())
}

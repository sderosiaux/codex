mod common;

use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerSource;
use codex_config::format_config_layer_source;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentConfigLayer;
use codex_exec_server::EnvironmentConfigLayerStack;
use codex_exec_server::EnvironmentConfigReadParams;
use codex_exec_server::EnvironmentConfigReadResponse;
use codex_exec_server::ExecServerError;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use common::exec_server::exec_server;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_environment_reads_projected_executor_config() -> anyhow::Result<()> {
    let mut server = exec_server().await?;
    let codex_home =
        AbsolutePathBuf::from_absolute_path(std::fs::canonicalize(server.codex_home())?)?;
    let config_file = codex_home.join(CONFIG_TOML_FILE);
    let project = codex_home.join("project");
    let dot_codex = project.join(".codex");
    tokio::fs::create_dir_all(dot_codex.as_path()).await?;
    tokio::fs::write(project.join(".project-root").as_path(), "").await?;
    let user_toml = "[shared_tools]\npath = \"./user-tools\"\n";
    tokio::fs::write(
        &config_file,
        format!("project_root_markers = [\".project-root\"]\n{user_toml}"),
    )
    .await?;
    tokio::fs::write(
        dot_codex.join(CONFIG_TOML_FILE).as_path(),
        r#"
[future_environment]
relative_path = "./executor-relative"
unselected = "do not return"
[features]
private_startup_value = "unselected-private-value"
"#,
    )
    .await?;

    let environment = Environment::create_for_tests(Some(server.websocket_url().to_string()))?;
    let environment_info = environment.info().await?;
    assert!(environment_info.capabilities.environment_config_read);
    assert_eq!(
        environment_info.user_home_dir,
        dirs::home_dir().and_then(|home_dir| PathUri::from_host_native_path(home_dir).ok()),
    );

    let response = environment
        .read_environment_config(EnvironmentConfigReadParams {
            cwd: PathUri::from_abs_path(&project),
            config_paths: vec![vec![
                "future_environment".to_string(),
                "relative_path".to_string(),
            ]],
        })
        .await?;

    let projected_toml = toml::toml! {
        [future_environment]
        relative_path = "./executor-relative"
    };
    assert_eq!(
        response,
        EnvironmentConfigReadResponse {
            user_home_dir: dirs::home_dir()
                .and_then(|home_dir| PathUri::from_host_native_path(home_dir).ok()),
            config: EnvironmentConfigLayerStack {
                layers: vec![EnvironmentConfigLayer {
                    source: format_config_layer_source(
                        &ConfigLayerSource::Project {
                            dot_codex_folder: dot_codex.clone(),
                        },
                        CONFIG_TOML_FILE,
                    ),
                    base_dir: PathUri::from_abs_path(&dot_codex),
                    toml: toml::to_string(&projected_toml)?,
                }],
            },
        }
    );

    let mut expected_user = response;
    expected_user.config.layers[0] = EnvironmentConfigLayer {
        source: format_config_layer_source(
            &ConfigLayerSource::User {
                file: config_file,
                profile: None,
            },
            CONFIG_TOML_FILE,
        ),
        base_dir: PathUri::from_abs_path(&codex_home),
        toml: toml::to_string(&toml::from_str::<toml::Value>(user_toml)?)?,
    };
    let user = environment
        .read_environment_config(EnvironmentConfigReadParams {
            cwd: PathUri::from_abs_path(&project),
            config_paths: vec![vec!["shared_tools".to_string()]],
        })
        .await?;
    assert_eq!(user, expected_user);
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_config_read_rejects_empty_selectors() -> anyhow::Result<()> {
    let mut server = exec_server().await?;
    let codex_home =
        AbsolutePathBuf::from_absolute_path(std::fs::canonicalize(server.codex_home())?)?;
    let environment = Environment::create_for_tests(Some(server.websocket_url().to_string()))?;

    for (config_paths, expected_message) in [
        (Vec::new(), "at least one config path is required"),
        (
            vec![Vec::new()],
            "TOML paths must contain at least one key segment",
        ),
    ] {
        let error = environment
            .read_environment_config(EnvironmentConfigReadParams {
                cwd: PathUri::from_abs_path(&codex_home),
                config_paths,
            })
            .await
            .expect_err("invalid selectors should fail");
        assert!(
            matches!(
                error,
                ExecServerError::Server { code: -32602, ref message }
                    if message == expected_message
            ),
            "unexpected error: {error:?}"
        );
    }

    server.shutdown().await?;
    Ok(())
}

use super::*;
use codex_config::ConfigLayerStack;
use codex_config::config_toml::ConfigToml;
use codex_config::config_toml::ForcedChatgptWorkspaceIds;
use codex_config::types::AuthCredentialsStoreMode;
use codex_features::FeaturesToml;
use codex_protocol::config_types::ForcedLoginMethod;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn resolve_bootstrap_auth_keyring_backend_kind_uses_secret_auth_storage_feature()
-> std::io::Result<()> {
    let config_toml = ConfigToml {
        features: Some(FeaturesToml::from(BTreeMap::from([(
            "secret_auth_storage".to_string(),
            true,
        )]))),
        ..Default::default()
    };
    assert_eq!(
        resolve_bootstrap_auth_keyring_backend_kind(&config_toml_load_result(config_toml)?)?,
        AuthKeyringBackendKind::Secrets
    );

    let config_toml = ConfigToml {
        features: Some(FeaturesToml::from(BTreeMap::from([(
            "secret_auth_storage".to_string(),
            false,
        )]))),
        ..Default::default()
    };
    assert_eq!(
        resolve_bootstrap_auth_keyring_backend_kind(&config_toml_load_result(config_toml)?)?,
        AuthKeyringBackendKind::Direct
    );

    Ok(())
}

#[test]
fn bootstrap_auth_config_uses_configured_login_restrictions() -> std::io::Result<()> {
    let config = ConfigToml {
        forced_login_method: Some(ForcedLoginMethod::Chatgpt),
        forced_chatgpt_workspace_id: Some(ForcedChatgptWorkspaceIds::Multiple(vec![
            " first ".to_string(),
            " ".to_string(),
            " second ".to_string(),
        ])),
        ..Default::default()
    };

    let auth_config =
        bootstrap_auth_config(Path::new("codex-home"), &config_toml_load_result(config)?)?;

    assert_eq!(
        auth_config.forced_login_method,
        Some(ForcedLoginMethod::Chatgpt)
    );
    assert!(auth_config.is_login_method_allowed(ForcedLoginMethod::Chatgpt));
    assert!(!auth_config.is_login_method_allowed(ForcedLoginMethod::Api));
    assert_eq!(
        auth_config.forced_chatgpt_workspace_id,
        Some(vec!["first".to_string(), "second".to_string()])
    );
    Ok(())
}

#[test]
fn bootstrap_auth_config_uses_configured_store_and_chatgpt_base_url() -> std::io::Result<()> {
    let configured_store = AuthCredentialsStoreMode::Keyring;
    let configured_url = "https://user.example/backend-api/";
    let config_toml = ConfigToml {
        cli_auth_credentials_store: Some(configured_store),
        chatgpt_base_url: Some(configured_url.to_string()),
        ..Default::default()
    };

    let auth_config = bootstrap_auth_config(
        Path::new("codex-home"),
        &config_toml_load_result(config_toml)?,
    )?;

    assert_eq!(auth_config.auth_credentials_store_mode, configured_store);
    assert_eq!(
        auth_config.chatgpt_base_url.as_deref(),
        Some(configured_url)
    );
    Ok(())
}

fn config_toml_load_result(config_toml: ConfigToml) -> std::io::Result<ConfigTomlLoadResult> {
    Ok(ConfigTomlLoadResult {
        config_toml,
        config_layer_stack: ConfigLayerStack::new(Vec::new())?,
    })
}

//! Exercises EMA activation from user configuration through real configuration layers.

use std::sync::Arc;

use anyhow::Result;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use tempfile::tempdir;
use test_case::test_case;
use wiremock::MockServer;

#[derive(Clone, Copy)]
enum ActivationScenario {
    FeatureDisabled,
    MissingIdp,
    OperatorEnabled,
}

#[test_case(ActivationScenario::FeatureDisabled; "feature disabled")]
#[test_case(ActivationScenario::MissingIdp; "missing IdP")]
#[test_case(ActivationScenario::OperatorEnabled; "configured registration is eligible for startup")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enterprise_activation_respects_configuration(scenario: ActivationScenario) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;
    let responses_server = responses::start_mock_server().await;
    let home = Arc::new(tempdir()?);
    let resource = format!("{}/mcp", server.uri());
    let issuer = format!("{}/idp", server.uri());
    let xaa_enabled = !matches!(scenario, ActivationScenario::FeatureDisabled);
    let mut user_config = format!(
        "mcp_oauth_credentials_store = \"file\"\n[features]\nuse_xaa = {xaa_enabled}\nsecret_auth_storage = false\napps = false\n"
    );
    if !matches!(scenario, ActivationScenario::MissingIdp) {
        user_config.push_str(&format!(
            "[mcp_enterprise_managed_auth.idp]\nissuer = {issuer:?}\nclient_id = \"idp-client\"\n"
        ));
    }
    user_config.push_str(&format!(
        "[mcp_servers.enterprise]\nurl = {resource:?}\nauth = \"ema_auth\"\n[mcp_servers.enterprise.oauth]\nclient_id = \"mcp-client\"\n"
    ));
    std::fs::write(home.path().join("config.toml"), user_config)?;
    let fixture = test_codex()
        .with_home(home)
        .with_auth(CodexAuth::from_api_key("test-api-key"))
        .build_with_auto_env(&responses_server)
        .await?;
    let (runtime_config, _) = fixture.codex.current_mcp_config_and_runtime_context().await;
    let expected_enabled = match scenario {
        ActivationScenario::FeatureDisabled | ActivationScenario::MissingIdp => Some(false),
        ActivationScenario::OperatorEnabled => Some(true),
    };
    assert_eq!(
        codex_mcp::configured_mcp_servers(&runtime_config)
            .get("enterprise")
            .map(|server| server.enabled),
        expected_enabled,
    );
    let startup = wait_for_event_match(&fixture.codex, |event| match event {
        EventMsg::McpStartupComplete(summary) => Some(summary.clone()),
        _ => None,
    })
    .await;
    // Eligible registration reaches startup, which fails closed without usable EMA auth.
    // Ineligible declarations must never start, even when their URLs are reachable.
    let expected_failed = if expected_enabled == Some(true) {
        vec!["enterprise".to_string()]
    } else {
        Vec::new()
    };
    assert_eq!(
        (
            startup.ready,
            startup
                .failed
                .into_iter()
                .map(|failure| failure.server)
                .collect::<Vec<_>>(),
            startup.cancelled,
        ),
        (Vec::<String>::new(), expected_failed, Vec::<String>::new()),
    );
    fixture.codex.shutdown_and_wait().await?;
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    Ok(())
}

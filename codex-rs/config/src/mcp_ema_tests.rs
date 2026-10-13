//! Enterprise registration credential-boundary checks.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn ema_rejects_alternate_credentials_and_executor_custody() {
    for extra in [
        "bearer_token_env_var='TOKEN'",
        "http_headers.Authorization='secret'",
        "http_headers.Accept='application/json'",
        "env_http_headers.X-Key='TOKEN'",
        "http_headers_helper='get-headers'",
        "oauth={client_id='client', client_secret='secret'}",
        "environment_id='remote'",
    ] {
        let server: McpServerConfig = toml::from_str(&format!(
            "url='https://resource.example'\nauth='ema_auth'\n{extra}"
        ))
        .unwrap();
        assert!(server.validate_ema_auth_transport().is_err(), "{extra}");
    }
    let server: McpServerConfig =
        toml::from_str("url='https://resource.example'\nauth='ema_auth'\nhttp_headers={}").unwrap();
    assert_eq!(server.validate_ema_auth_transport(), Ok(()));
}

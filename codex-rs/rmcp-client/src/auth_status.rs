use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_exec_server::HttpClient;
use codex_protocol::protocol::McpAuthStatus;
use futures::FutureExt;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use tracing::debug;

use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::StoredOAuthTokenStatus;
use crate::oauth::oauth_token_status;
use crate::oauth_callback::McpOAuthCallbackMode;
use crate::oauth_callback::callback_mode;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout policy for OAuth metadata discovery through a supplied HTTP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthDiscoveryTimeout {
    /// Preserve the timeout requested by the OAuth implementation.
    Requested,
    /// Cap OAuth discovery requests at the supplied duration.
    Capped(Duration),
}

impl OAuthDiscoveryTimeout {
    /// Preserves the existing timeout for local OAuth discovery.
    pub const LOCAL: Self = Self::Capped(DISCOVERY_TIMEOUT);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamableHttpOAuthDiscovery {
    pub scopes_supported: Option<Vec<String>>,
    pub callback_mode: McpOAuthCallbackMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoginRequirement {
    Login,
    Reauthentication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthState {
    Unsupported,
    Unknown,
    LoggedOut(McpLoginRequirement),
    BearerToken,
    OAuth,
}

impl From<McpAuthState> for McpAuthStatus {
    fn from(value: McpAuthState) -> Self {
        match value {
            McpAuthState::Unsupported => Self::Unsupported,
            McpAuthState::Unknown => Self::Unknown,
            McpAuthState::LoggedOut(_) => Self::NotLoggedIn,
            McpAuthState::BearerToken => Self::BearerToken,
            McpAuthState::OAuth => Self::OAuth,
        }
    }
}

enum AuthStatusCheck {
    Complete(McpAuthState),
    Discover(HeaderMap),
}

/// Determine authentication status while routing OAuth discovery through the
/// provided HTTP client.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<McpAuthState> {
    let has_configured_headers = has_configured_headers(&http_headers, &env_http_headers);
    let default_headers = match auth_status_before_discovery(
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )? {
        AuthStatusCheck::Complete(status) => return Ok(status),
        AuthStatusCheck::Discover(default_headers) => default_headers,
    };
    determine_auth_status_from_discovery(
        server_name,
        url,
        discover_streamable_http_oauth_with_headers_and_http_client(
            url,
            default_headers,
            http_client,
            discovery_timeout,
            has_configured_headers,
            redirect_mode,
        )
        .await,
    )
}

/// Determine authentication status using only configured and stored credentials.
///
/// Returns `None` when determining the status would require OAuth metadata discovery.
pub fn determine_streamable_http_auth_status_from_credentials(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<McpAuthState>> {
    match auth_status_before_discovery(
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )? {
        AuthStatusCheck::Complete(status) => Ok(Some(status)),
        AuthStatusCheck::Discover(_) => Ok(None),
    }
}

fn auth_status_before_discovery(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<AuthStatusCheck> {
    if bearer_token_env_var.is_some() {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    if default_headers.contains_key(AUTHORIZATION) {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    match oauth_token_status(server_name, url, store_mode, keyring_backend_kind)? {
        StoredOAuthTokenStatus::Usable => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::OAuth));
        }
        StoredOAuthTokenStatus::AuthorizationRequired => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::LoggedOut(
                McpLoginRequirement::Reauthentication,
            )));
        }
        StoredOAuthTokenStatus::Missing => {}
    }

    Ok(AuthStatusCheck::Discover(default_headers))
}

fn determine_auth_status_from_discovery(
    server_name: &str,
    url: &str,
    discovery: Result<Option<StreamableHttpOAuthDiscovery>>,
) -> Result<McpAuthState> {
    match discovery {
        Ok(Some(_)) => Ok(McpAuthState::LoggedOut(McpLoginRequirement::Login)),
        Ok(None) => Ok(McpAuthState::Unsupported),
        Err(error) => {
            debug!(
                "failed to detect OAuth support for MCP server `{server_name}` at {url}: {error:?}"
            );
            Err(error)
        }
    }
}

pub async fn discover_streamable_http_oauth(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let has_configured_headers = has_configured_headers(&http_headers, &env_http_headers);
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers_and_http_client(
        url,
        default_headers,
        http_client,
        discovery_timeout,
        has_configured_headers,
        redirect_mode,
    )
    .await
}

async fn discover_streamable_http_oauth_with_headers_and_http_client(
    url: &str,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    has_configured_headers: bool,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let oauth_http_client = match discovery_timeout {
        OAuthDiscoveryTimeout::Requested => OAuthHttpClientAdapter::new_with_redirect_mode(
            http_client,
            default_headers,
            url,
            has_configured_headers,
            redirect_mode,
        )?,
        OAuthDiscoveryTimeout::Capped(max_timeout) => {
            OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
                http_client,
                default_headers,
                url,
                max_timeout,
                has_configured_headers,
                redirect_mode,
            )?
        }
    };
    let mut authorization_manager =
        AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http_client)).await?;
    authorization_manager.set_allow_missing_issuer(true);
    discover_streamable_http_oauth_with_manager(&authorization_manager).await
}

fn has_configured_headers(
    http_headers: &Option<HashMap<String, String>>,
    env_http_headers: &Option<HashMap<String, String>>,
) -> bool {
    http_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
        || env_http_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty())
}

async fn discover_streamable_http_oauth_with_manager(
    authorization_manager: &AuthorizationManager,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    match authorization_manager.resolve_metadata().boxed().await {
        Ok(resolution) if !resolution.source.is_discovered() => Ok(None),
        Ok(resolution) => {
            let metadata = resolution.metadata;
            Ok(Some(StreamableHttpOAuthDiscovery {
                callback_mode: callback_mode(&metadata)
                    .unwrap_or(McpOAuthCallbackMode::CallbackSpecific),
                scopes_supported: normalize_scopes(metadata.scopes_supported),
            }))
        }
        Err(AuthError::NoAuthorizationSupport) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn normalize_scopes(scopes_supported: Option<Vec<String>>) -> Option<Vec<String>> {
    let scopes_supported = scopes_supported?;

    let mut normalized = Vec::new();
    for scope in scopes_supported {
        let scope = scope.trim();
        if scope.is_empty() {
            continue;
        }
        let scope = scope.to_string();
        if !normalized.contains(&scope) {
            normalized.push(scope);
        }
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

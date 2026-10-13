mod workspace_routing;

use chrono::Utc;
use http::StatusCode;
use serde::Deserialize;
use std::env;
use std::fmt::Debug;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::Weak;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tracing::instrument;

use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;

use super::change_state::AuthChangeState;
use super::change_state::same_owner;
use super::revoke::revoke_auth_tokens;
pub use crate::auth::storage::AuthDotJson;
pub use crate::auth::storage::AuthKeyringBackendKind;
use crate::auth::storage::AuthStorageBackend;
use crate::auth::storage::create_auth_storage;
use crate::default_client::create_default_auth_client;
use crate::oauth::OAuthClient;
use crate::oauth::OAuthError;
use crate::oauth::RefreshTokenGrant;
use crate::oauth::TokenEncoding;
use crate::oauth::TokenEndpoint;
use crate::oauth::TokenErrorDetail;
use crate::outbound_proxy::AuthRouteConfig;
use crate::token_data::TokenData;
use crate::token_data::parse_chatgpt_account_user_id;
use crate::token_data::parse_chatgpt_jwt_claims;
use crate::token_data::parse_jwt_expiration;
use codex_config::types::AuthCredentialsStoreMode;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::RefreshTokenFailedError;
use codex_protocol::auth::RefreshTokenFailedReason;
use thiserror::Error;
pub use workspace_routing::WorkspaceRouting;
pub use workspace_routing::WorkspaceRoutingRequest;
pub use workspace_routing::WorkspaceRoutingResolver;
pub use workspace_routing::WorkspaceRoutingSession;

/// Authentication mechanism used by the current user.
#[derive(Debug, Clone)]
pub enum CodexAuth {
    ApiKey(ApiKeyAuth),
    Chatgpt(ChatgptAuth),
}

impl PartialEq for CodexAuth {
    fn eq(&self, other: &Self) -> bool {
        self.auth_mode() == other.auth_mode()
    }
}

#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    api_key: String,
}

#[derive(Debug, Clone)]
pub struct ChatgptAuth {
    state: ChatgptAuthState,
    storage: Arc<dyn AuthStorageBackend>,
}

#[derive(Debug, Clone)]
struct ChatgptAuthState {
    auth_dot_json: Arc<Mutex<Option<AuthDotJson>>>,
    client: HttpClient,
}

const TOKEN_REFRESH_INTERVAL: i64 = 8;
const CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES: i64 = 5;

const REFRESH_TOKEN_EXPIRED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.";
const REFRESH_TOKEN_REUSED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.";
const REFRESH_TOKEN_INVALIDATED_MESSAGE: &str = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.";
const REFRESH_TOKEN_UNKNOWN_MESSAGE: &str =
    "Your access token could not be refreshed. Please log out and sign in again.";
const REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE: &str = "Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.";
const REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(super) const REVOKE_TOKEN_URL: &str = "https://auth.openai.com/oauth/revoke";
pub const REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
pub const REVOKE_TOKEN_URL_OVERRIDE_ENV_VAR: &str = "CODEX_REVOKE_TOKEN_URL_OVERRIDE";
pub const CLIENT_ID_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_CLIENT_ID";

#[derive(Debug, Error)]
pub enum RefreshTokenError {
    #[error("{0}")]
    Permanent(#[from] RefreshTokenFailedError),
    #[error(transparent)]
    Transient(#[from] std::io::Error),
}

impl RefreshTokenError {
    pub fn failed_reason(&self) -> Option<RefreshTokenFailedReason> {
        match self {
            Self::Permanent(error) => Some(error.reason),
            Self::Transient(_) => None,
        }
    }
}

impl From<codex_http_client::HttpError> for RefreshTokenError {
    fn from(error: codex_http_client::HttpError) -> Self {
        Self::Transient(std::io::Error::other(error))
    }
}

impl From<RefreshTokenError> for std::io::Error {
    fn from(err: RefreshTokenError) -> Self {
        match err {
            RefreshTokenError::Permanent(failed) => std::io::Error::other(failed),
            RefreshTokenError::Transient(inner) => inner,
        }
    }
}

impl CodexAuth {
    fn from_auth_dot_json(
        codex_home: &Path,
        auth_dot_json: AuthDotJson,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<Self> {
        if auth_dot_json.resolved_mode() == AuthMode::ApiKey {
            let Some(api_key) = auth_dot_json.openai_api_key.as_deref() else {
                return Err(std::io::Error::other("API key auth is missing a key."));
            };
            return Ok(Self::from_api_key(api_key));
        }

        let client = create_default_auth_client(&refresh_token_endpoint(), auth_route_config)?;
        let state = ChatgptAuthState {
            auth_dot_json: Arc::new(Mutex::new(Some(auth_dot_json))),
            client,
        };
        let storage = create_auth_storage(
            codex_home.to_path_buf(),
            auth_credentials_store_mode,
            keyring_backend_kind,
        );
        Ok(Self::Chatgpt(ChatgptAuth { state, storage }))
    }

    pub fn from_auth_storage(
        codex_home: &Path,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: &AuthRouteConfig,
    ) -> std::io::Result<Option<Self>> {
        load_auth(
            codex_home,
            /*enable_codex_api_key_env*/ false,
            auth_credentials_store_mode,
            /*allowed_login_methods*/ None,
            keyring_backend_kind,
            auth_route_config,
        )
    }

    /// Returns the kind of credentials backing this authentication.
    pub fn auth_mode(&self) -> AuthMode {
        match self {
            Self::ApiKey(_) => AuthMode::ApiKey,
            Self::Chatgpt(_) => AuthMode::Chatgpt,
        }
    }

    pub fn is_api_key_auth(&self) -> bool {
        self.auth_mode() == AuthMode::ApiKey
    }

    pub fn is_chatgpt_auth(&self) -> bool {
        self.auth_mode() == AuthMode::Chatgpt
    }

    /// ChatGPT auth talks to the Codex backend; API keys talk to the OpenAI API directly.
    pub fn uses_codex_backend(&self) -> bool {
        self.is_chatgpt_auth()
    }

    /// Returns `None` if `auth_mode() != AuthMode::ApiKey`.
    pub fn api_key(&self) -> Option<&str> {
        match self {
            Self::ApiKey(auth) => Some(auth.api_key.as_str()),
            Self::Chatgpt(_) => None,
        }
    }

    /// Returns `Err` if token-backed ChatGPT auth is unavailable.
    pub fn get_token_data(&self) -> Result<TokenData, std::io::Error> {
        let auth_dot_json: Option<AuthDotJson> = self.get_current_auth_json();
        match auth_dot_json {
            Some(AuthDotJson {
                tokens: Some(tokens),
                last_refresh: Some(_),
                ..
            }) => Ok(tokens),
            _ => Err(std::io::Error::other("Token data is not available.")),
        }
    }

    /// Returns the token string used for bearer authentication.
    pub fn get_token(&self) -> Result<String, std::io::Error> {
        match self {
            Self::ApiKey(auth) => Ok(auth.api_key.clone()),
            Self::Chatgpt(_) => Ok(self.get_token_data()?.access_token),
        }
    }

    /// Returns `None` if Codex backend auth does not expose an account id.
    pub fn get_account_id(&self) -> Option<String> {
        self.get_current_token_data().and_then(|t| t.account_id)
    }

    /// Returns false if Codex backend auth omits the FedRAMP claim.
    pub fn is_fedramp_account(&self) -> bool {
        self.get_current_token_data()
            .is_some_and(|t| t.id_token.is_fedramp_account())
    }

    /// Returns `None` if Codex backend auth does not expose an account email.
    pub fn get_account_email(&self) -> Option<String> {
        self.get_current_token_data().and_then(|t| t.id_token.email)
    }

    /// Returns `None` if Codex backend auth does not expose a ChatGPT user id.
    pub fn get_chatgpt_user_id(&self) -> Option<String> {
        self.get_current_token_data()
            .and_then(|t| t.id_token.chatgpt_user_id)
    }

    /// Returns the access token's opaque account-user identity only when its workspace
    /// matches the selected account. Missing claims never fall back to a user id.
    /// Unlike `get_chatgpt_user_id`, this identifies one workspace membership, so keys
    /// are not shared across a person's workspaces. This is local identity selection, not access-token or proof validation.
    pub fn get_chatgpt_account_user_id(&self) -> Option<String> {
        let tokens = self.get_current_token_data()?;
        parse_chatgpt_account_user_id(&tokens.access_token, tokens.account_id.as_deref()?)
            .ok()
            .flatten()
    }

    /// Account-facing plan classification derived from the current auth.
    /// Returns a high-level `AccountPlanType` (e.g., Free/Plus/Pro/Team/…)
    /// for UI or product decisions based on the user's subscription.
    pub fn account_plan_type(&self) -> Option<AccountPlanType> {
        self.get_current_token_data().map(|t| {
            t.id_token
                .chatgpt_plan_type
                .map(AccountPlanType::from)
                .unwrap_or(AccountPlanType::Unknown)
        })
    }

    pub fn is_workspace_account(&self) -> bool {
        self.account_plan_type()
            .is_some_and(AccountPlanType::is_workspace_account)
    }

    /// Returns `None` if token-backed ChatGPT auth is unavailable.
    fn get_current_auth_json(&self) -> Option<AuthDotJson> {
        let state = match self {
            Self::Chatgpt(auth) => &auth.state,
            Self::ApiKey(_) => return None,
        };
        #[expect(clippy::unwrap_used)]
        state.auth_dot_json.lock().unwrap().clone()
    }

    /// Returns `None` if token-backed ChatGPT auth is unavailable.
    fn get_current_token_data(&self) -> Option<TokenData> {
        self.get_current_auth_json().and_then(|t| t.tokens)
    }

    pub fn from_api_key(api_key: &str) -> Self {
        Self::ApiKey(ApiKeyAuth {
            api_key: api_key.to_owned(),
        })
    }
}

impl ChatgptAuth {
    fn current_auth_json(&self) -> Option<AuthDotJson> {
        #[expect(clippy::unwrap_used)]
        self.state.auth_dot_json.lock().unwrap().clone()
    }

    fn current_token_data(&self) -> Option<TokenData> {
        self.current_auth_json().and_then(|auth| auth.tokens)
    }

    fn storage(&self) -> &Arc<dyn AuthStorageBackend> {
        &self.storage
    }

    fn client(&self) -> &HttpClient {
        &self.state.client
    }
}

pub const OPENAI_API_KEY_ENV_VAR: &str = "OPENAI_API_KEY";
pub const CODEX_API_KEY_ENV_VAR: &str = "CODEX_API_KEY";

pub fn read_openai_api_key_from_env() -> Option<String> {
    env::var(OPENAI_API_KEY_ENV_VAR)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn read_codex_api_key_from_env() -> Option<String> {
    read_non_empty_env_var(CODEX_API_KEY_ENV_VAR)
}

fn read_non_empty_env_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Delete credentials for `codex_home` through the selected storage backend.
/// Returns `Ok(true)` if credentials were removed, `Ok(false)` if none were present.
pub fn logout(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<bool> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.delete()
}

pub async fn logout_with_revoke(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<bool> {
    let auth_dot_json = match load_auth_dot_json(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    ) {
        Ok(auth_dot_json) => auth_dot_json,
        Err(err) => {
            tracing::warn!("failed to load stored auth during logout: {err}");
            None
        }
    };
    if let Err(err) = revoke_auth_tokens(auth_dot_json.as_ref(), auth_route_config).await {
        tracing::warn!("failed to revoke auth tokens during logout: {err}");
    }
    logout_all_stores(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )
}

/// Store API-key credentials through the selected storage backend.
pub fn login_with_api_key(
    codex_home: &Path,
    api_key: &str,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    let auth_dot_json = AuthDotJson {
        auth_mode: Some(AuthMode::ApiKey),
        openai_api_key: Some(api_key.to_string()),
        tokens: None,
        last_refresh: None,
    };
    save_auth(
        codex_home,
        &auth_dot_json,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )
}

/// Persist the provided auth payload using the specified backend.
pub fn save_auth(
    codex_home: &Path,
    auth: &AuthDotJson,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.save(auth)
}

/// Load the raw stored auth payload without applying environment overrides.
///
/// Returns `None` when no credentials are stored. Prefer `AuthManager` for
/// ordinary production reads; this helper is for tests and write-side
/// maintenance that must inspect the exact payload in storage.
pub fn load_auth_dot_json(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<Option<AuthDotJson>> {
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    storage.load()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub codex_home: PathBuf,
    pub auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub keyring_backend_kind: AuthKeyringBackendKind,
    pub forced_login_method: Option<ForcedLoginMethod>,
    pub chatgpt_base_url: Option<String>,
    pub forced_chatgpt_workspace_id: Option<Vec<String>>,
    pub auth_route_config: AuthRouteConfig,
}

impl AuthConfig {
    pub fn is_login_method_allowed(&self, method: ForcedLoginMethod) -> bool {
        login_method_allowed(
            method,
            self.forced_login_method,
            self.forced_chatgpt_workspace_id.as_deref(),
        )
    }

    pub fn effective_chatgpt_workspaces(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id.clone()
    }

    pub fn validate(&self) -> std::io::Result<()> {
        if self.is_login_method_allowed(ForcedLoginMethod::Api)
            || self.is_login_method_allowed(ForcedLoginMethod::Chatgpt)
        {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "authentication requirements do not permit any usable login method",
            ))
        }
    }

    pub fn allows_auth(&self, auth: &CodexAuth) -> bool {
        let allowed_login_methods = self.allowed_login_methods();
        let workspaces = self.effective_chatgpt_workspaces();
        validate_auth_restrictions(Some(&allowed_login_methods), workspaces.as_deref(), auth)
            .is_ok()
    }

    pub fn load_auth(&self, enable_codex_api_key_env: bool) -> std::io::Result<Option<CodexAuth>> {
        let allowed_login_methods = self.allowed_login_methods();
        let auth = load_auth(
            &self.codex_home,
            enable_codex_api_key_env,
            self.auth_credentials_store_mode,
            Some(&allowed_login_methods),
            self.keyring_backend_kind,
            &self.auth_route_config,
        )?;
        Ok(auth.filter(|auth| self.allows_auth(auth)))
    }

    fn allowed_login_methods(&self) -> Vec<ForcedLoginMethod> {
        allowed_login_methods(
            self.forced_login_method,
            self.forced_chatgpt_workspace_id.as_deref(),
        )
    }
}

/// A ChatGPT login is only usable when the configured workspace restriction is non-empty.
fn login_method_allowed(
    method: ForcedLoginMethod,
    forced_login_method: Option<ForcedLoginMethod>,
    forced_workspaces: Option<&[String]>,
) -> bool {
    forced_login_method.is_none_or(|forced| forced == method)
        && (method != ForcedLoginMethod::Chatgpt
            || forced_workspaces.is_none_or(|workspaces| !workspaces.is_empty()))
}

fn allowed_login_methods(
    forced_login_method: Option<ForcedLoginMethod>,
    forced_workspaces: Option<&[String]>,
) -> Vec<ForcedLoginMethod> {
    [ForcedLoginMethod::Api, ForcedLoginMethod::Chatgpt]
        .into_iter()
        .filter(|method| login_method_allowed(*method, forced_login_method, forced_workspaces))
        .collect()
}

fn auth_mode_is_allowed(
    allowed_login_methods: Option<&[ForcedLoginMethod]>,
    mode: AuthMode,
) -> bool {
    let method = if mode.uses_codex_backend() {
        ForcedLoginMethod::Chatgpt
    } else {
        ForcedLoginMethod::Api
    };
    allowed_login_methods.is_none_or(|allowed| allowed.contains(&method))
}

fn validate_auth_restrictions(
    allowed_login_methods: Option<&[ForcedLoginMethod]>,
    expected_workspaces: Option<&[String]>,
    auth: &CodexAuth,
) -> Result<(), String> {
    if !auth_mode_is_allowed(allowed_login_methods, auth.auth_mode()) {
        return Err(match allowed_login_methods {
            Some(methods) if methods.contains(&ForcedLoginMethod::Api) => {
                "API key login is required".to_string()
            }
            Some(_) => "ChatGPT login is required".to_string(),
            None => unreachable!("unrestricted login methods accept every auth mode"),
        });
    }

    let Some(expected_workspaces) = expected_workspaces else {
        return Ok(());
    };
    if auth.is_api_key_auth() {
        return Ok(());
    }

    let actual_workspace = auth.get_account_id().or_else(|| {
        auth.get_token_data()
            .ok()
            .and_then(|tokens| tokens.id_token.chatgpt_account_id)
    });
    if actual_workspace
        .as_ref()
        .is_some_and(|workspace| expected_workspaces.contains(workspace))
    {
        Ok(())
    } else {
        let actual = actual_workspace.unwrap_or_else(|| "unknown".to_string());
        Err(format!(
            "Login is restricted to workspace(s) {}, but current credentials belong to {actual}",
            expected_workspaces.join(", ")
        ))
    }
}

/// Enforces configured login restrictions using auth-owned HTTP settings.
pub fn enforce_login_restrictions(config: &AuthConfig) -> std::io::Result<()> {
    // Managed-only restrictions are enforced by AuthManager.
    if config.forced_login_method.is_none() && config.forced_chatgpt_workspace_id.is_none() {
        return Ok(());
    }

    let Some(auth) = load_auth(
        &config.codex_home,
        /*enable_codex_api_key_env*/ true,
        config.auth_credentials_store_mode,
        /*allowed_login_methods*/ None,
        config.keyring_backend_kind,
        &config.auth_route_config,
    )?
    else {
        return Ok(());
    };

    if let Some(required_method) = config.forced_login_method {
        let method_violation = match (required_method, auth.auth_mode()) {
            (ForcedLoginMethod::Api, AuthMode::ApiKey)
            | (ForcedLoginMethod::Chatgpt, AuthMode::Chatgpt) => None,
            (ForcedLoginMethod::Api, AuthMode::Chatgpt) => Some(
                "API key login is required, but ChatGPT is currently being used. Logging out."
                    .to_string(),
            ),
            (ForcedLoginMethod::Chatgpt, AuthMode::ApiKey) => Some(
                "ChatGPT login is required, but an API key is currently being used. Logging out."
                    .to_string(),
            ),
        };

        if let Some(message) = method_violation {
            return logout_with_message(
                &config.codex_home,
                message,
                config.auth_credentials_store_mode,
                config.keyring_backend_kind,
            );
        }
    }

    if let Some(expected_account_ids) = config.forced_chatgpt_workspace_id.as_deref() {
        let chatgpt_account_id = match &auth {
            CodexAuth::ApiKey(_) => {
                return Ok(());
            }
            CodexAuth::Chatgpt(_) => {
                let token_data = match auth.get_token_data() {
                    Ok(data) => data,
                    Err(err) => {
                        return logout_with_message(
                            &config.codex_home,
                            format!(
                                "Failed to load ChatGPT credentials while enforcing workspace restrictions: {err}. Logging out."
                            ),
                            config.auth_credentials_store_mode,
                            config.keyring_backend_kind,
                        );
                    }
                };
                token_data.id_token.chatgpt_account_id
            }
        };

        // workspace is the external identifier for account id.
        let chatgpt_account_id = chatgpt_account_id.as_deref();
        if !chatgpt_account_id.is_some_and(|actual| {
            expected_account_ids
                .iter()
                .any(|expected| expected == actual)
        }) {
            let expected_workspaces = expected_account_ids.join(", ");
            let message = match chatgpt_account_id {
                Some(actual) => format!(
                    "Login is restricted to workspace(s) {expected_workspaces}, but current credentials belong to {actual}. Logging out."
                ),
                None => format!(
                    "Login is restricted to workspace(s) {expected_workspaces}, but current credentials lack a workspace identifier. Logging out."
                ),
            };
            return logout_with_message(
                &config.codex_home,
                message,
                config.auth_credentials_store_mode,
                config.keyring_backend_kind,
            );
        }
    }

    Ok(())
}

fn logout_with_message(
    codex_home: &Path,
    message: String,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<()> {
    // External auth tokens live in the ephemeral store, but persistent auth may still exist
    // from earlier logins. Clear both so a forced logout truly removes all active auth.
    let removal_result = logout_all_stores(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    let error_message = match removal_result {
        Ok(_) => message,
        Err(err) => format!("{message}. Failed to remove stored credentials: {err}"),
    };
    Err(std::io::Error::other(error_message))
}

fn logout_all_stores(
    codex_home: &Path,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> std::io::Result<bool> {
    if auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
        return logout(
            codex_home,
            AuthCredentialsStoreMode::Ephemeral,
            AuthKeyringBackendKind::default(),
        );
    }
    let removed_ephemeral = logout(
        codex_home,
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::default(),
    )?;
    let removed_managed = logout(
        codex_home,
        auth_credentials_store_mode,
        keyring_backend_kind,
    )?;
    Ok(removed_ephemeral || removed_managed)
}

fn load_auth(
    codex_home: &Path,
    enable_codex_api_key_env: bool,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    allowed_login_methods: Option<&[ForcedLoginMethod]>,
    keyring_backend_kind: AuthKeyringBackendKind,
    auth_route_config: &AuthRouteConfig,
) -> std::io::Result<Option<CodexAuth>> {
    // API key via env var takes precedence over any other auth method.
    if enable_codex_api_key_env
        && auth_mode_is_allowed(allowed_login_methods, AuthMode::ApiKey)
        && let Some(api_key) = read_codex_api_key_from_env()
    {
        return Ok(Some(CodexAuth::from_api_key(api_key.as_str())));
    }

    // In-memory (ephemeral) credentials take precedence over persisted ones.
    let ephemeral_storage = create_auth_storage(
        codex_home.to_path_buf(),
        AuthCredentialsStoreMode::Ephemeral,
        AuthKeyringBackendKind::default(),
    );
    if let Some(auth_dot_json) = ephemeral_storage.load()?
        && auth_mode_is_allowed(allowed_login_methods, auth_dot_json.resolved_mode())
    {
        return CodexAuth::from_auth_dot_json(
            codex_home,
            auth_dot_json,
            AuthCredentialsStoreMode::Ephemeral,
            keyring_backend_kind,
            auth_route_config,
        )
        .map(Some);
    }

    // If the caller explicitly requested ephemeral auth, there is no persisted fallback.
    if auth_credentials_store_mode == AuthCredentialsStoreMode::Ephemeral {
        return Ok(None);
    }

    // Fall back to the configured persistent store (file/keyring/auto) for managed auth.
    let storage = create_auth_storage(
        codex_home.to_path_buf(),
        auth_credentials_store_mode,
        keyring_backend_kind,
    );
    let Some(auth_dot_json) = storage.load()? else {
        return Ok(None);
    };
    if !auth_mode_is_allowed(allowed_login_methods, auth_dot_json.resolved_mode()) {
        return Ok(None);
    }
    CodexAuth::from_auth_dot_json(
        codex_home,
        auth_dot_json,
        auth_credentials_store_mode,
        keyring_backend_kind,
        auth_route_config,
    )
    .map(Some)
}

// Persist refreshed tokens into auth storage and update last_refresh.
fn persist_tokens(
    storage: &Arc<dyn AuthStorageBackend>,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
) -> std::io::Result<AuthDotJson> {
    let mut auth_dot_json = storage
        .load()?
        .ok_or(std::io::Error::other("Token data is not available."))?;

    let tokens = auth_dot_json.tokens.get_or_insert_with(TokenData::default);
    if let Some(id_token) = id_token {
        tokens.id_token = parse_chatgpt_jwt_claims(&id_token).map_err(std::io::Error::other)?;
    }
    if let Some(access_token) = access_token {
        tokens.access_token = access_token;
    }
    if let Some(refresh_token) = refresh_token {
        tokens.refresh_token = refresh_token;
    }
    auth_dot_json.last_refresh = Some(Utc::now());
    storage.save(&auth_dot_json)?;
    Ok(auth_dot_json)
}

// Requests refreshed ChatGPT OAuth tokens from the auth service using a refresh token.
// The caller is responsible for persisting any returned tokens.
async fn request_chatgpt_token_refresh(
    refresh_token: String,
    client: &HttpClient,
) -> Result<RefreshResponse, RefreshTokenError> {
    let client_id = oauth_client_id();
    let endpoint = refresh_token_endpoint();
    let oauth = OAuthClient::new(
        client,
        TokenEndpoint {
            url: &endpoint,
            client_id: &client_id,
            encoding: TokenEncoding::Json,
            timeout: None,
        },
    );
    match oauth
        .refresh(RefreshTokenGrant {
            refresh_token: &refresh_token,
            resource: None,
        })
        .await
    {
        Ok(response) => Ok(response),
        Err(OAuthError::Rejected(rejection)) => {
            let status = rejection.status;
            let detail = &rejection.detail;
            tracing::error!(%status, ?detail, "Failed to refresh token");
            let code = detail.error_code();
            let is_invalid_grant_bad_request = status == StatusCode::BAD_REQUEST
                && code.is_some_and(|code| code.eq_ignore_ascii_case("invalid_grant"));
            let failed = classify_refresh_token_failure(code, detail, is_invalid_grant_bad_request);
            if status == StatusCode::UNAUTHORIZED
                || failed.reason != RefreshTokenFailedReason::Other
                || is_invalid_grant_bad_request
            {
                Err(RefreshTokenError::Permanent(failed))
            } else {
                Err(RefreshTokenError::Transient(std::io::Error::other(
                    format!("Failed to refresh token: {status}: {detail}"),
                )))
            }
        }
        Err(OAuthError::Transport(error)) => Err(error.into()),
        Err(error @ OAuthError::InvalidResponse) => {
            Err(RefreshTokenError::Transient(std::io::Error::other(error)))
        }
    }
}

fn classify_refresh_token_failure(
    code: Option<&str>,
    detail: &TokenErrorDetail,
    is_invalid_grant_bad_request: bool,
) -> RefreshTokenFailedError {
    let normalized_code = code.map(str::to_ascii_lowercase);
    let reason = match normalized_code.as_deref() {
        Some("refresh_token_expired") => RefreshTokenFailedReason::Expired,
        Some("refresh_token_reused") => RefreshTokenFailedReason::Exhausted,
        Some("refresh_token_invalidated") => RefreshTokenFailedReason::Revoked,
        _ => RefreshTokenFailedReason::Other,
    };

    if reason == RefreshTokenFailedReason::Other && !is_invalid_grant_bad_request {
        tracing::warn!(
            backend_detail = ?detail,
            "Encountered unknown response while refreshing token"
        );
    }

    let message = match reason {
        RefreshTokenFailedReason::Expired => REFRESH_TOKEN_EXPIRED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Exhausted => REFRESH_TOKEN_REUSED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Revoked => REFRESH_TOKEN_INVALIDATED_MESSAGE.to_string(),
        RefreshTokenFailedReason::Other => REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
    };

    RefreshTokenFailedError::new(reason, message)
}

#[derive(Deserialize, Clone)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

// Shared constant for token refresh (client id used for oauth token refresh flow)
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn oauth_client_id() -> String {
    std::env::var(CLIENT_ID_OVERRIDE_ENV_VAR)
        .ok()
        .filter(|client_id| !client_id.trim().is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

fn refresh_token_endpoint() -> String {
    std::env::var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR)
        .unwrap_or_else(|_| REFRESH_TOKEN_URL.to_string())
}

impl AuthDotJson {
    pub(super) fn resolved_mode(&self) -> AuthMode {
        if let Some(mode) = self.auth_mode {
            return mode;
        }
        if self.openai_api_key.is_some() {
            return AuthMode::ApiKey;
        }
        AuthMode::Chatgpt
    }
}

/// Internal cached auth state.
#[derive(Clone)]
struct CachedAuth {
    auth: Option<CodexAuth>,
    /// Permanent refresh failure cached for the current auth snapshot so
    /// later refresh attempts for the same credentials fail fast without network.
    permanent_refresh_failure: Option<AuthScopedRefreshFailure>,
}

#[derive(Clone)]
struct AuthScopedRefreshFailure {
    auth: CodexAuth,
    error: RefreshTokenFailedError,
}

impl Debug for CachedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedAuth")
            .field("auth_mode", &self.auth.as_ref().map(CodexAuth::auth_mode))
            .field(
                "permanent_refresh_failure",
                &self
                    .permanent_refresh_failure
                    .as_ref()
                    .map(|failure| failure.error.reason),
            )
            .finish()
    }
}

enum UnauthorizedRecoveryStep {
    Reload,
    RefreshToken,
    Done,
}

enum ReloadOutcome {
    /// Reload was performed and the cached auth changed
    ReloadedChanged,
    /// Reload was performed and the cached auth remained the same
    ReloadedNoChange,
    /// Reload was skipped (missing or mismatched account id)
    Skipped,
}

// UnauthorizedRecovery is a state machine that handles an attempt to refresh the authentication when requests
// to API fail with 401 status code.
// The client calls next() every time it encounters a 401 error, one time per retry.
// For API key based authentication, we don't do anything and let the error bubble to the user.
//
// For ChatGPT based authentication, we:
// 1. Attempt to reload the auth data from disk. We only reload if the account id matches the one the current process is running as.
// 2. Attempt to refresh the token using OAuth token refresh flow.
// If after both steps the server still responds with 401 we let the error bubble to the user.
pub struct UnauthorizedRecovery {
    manager: Arc<AuthManager>,
    step: UnauthorizedRecoveryStep,
    expected_account_id: Option<String>,
}

impl UnauthorizedRecovery {
    fn new(manager: Arc<AuthManager>) -> Self {
        let expected_account_id = manager
            .auth_cached()
            .as_ref()
            .and_then(CodexAuth::get_account_id);
        Self {
            manager,
            step: UnauthorizedRecoveryStep::Reload,
            expected_account_id,
        }
    }

    pub fn has_next(&self) -> bool {
        self.manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::is_chatgpt_auth)
            && !matches!(self.step, UnauthorizedRecoveryStep::Done)
    }

    pub async fn next(&mut self) -> Result<(), RefreshTokenError> {
        if !self.has_next() {
            return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                "No more recovery steps available.",
            )));
        }

        match self.step {
            UnauthorizedRecoveryStep::Reload => {
                match self
                    .manager
                    .reload_if_account_id_matches(self.expected_account_id.as_deref())
                    .await
                {
                    ReloadOutcome::ReloadedChanged | ReloadOutcome::ReloadedNoChange => {
                        self.step = UnauthorizedRecoveryStep::RefreshToken;
                    }
                    ReloadOutcome::Skipped => {
                        self.step = UnauthorizedRecoveryStep::Done;
                        return Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                            RefreshTokenFailedReason::Other,
                            REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE.to_string(),
                        )));
                    }
                }
            }
            UnauthorizedRecoveryStep::RefreshToken => {
                self.manager.refresh_token_from_authority().await?;
                self.step = UnauthorizedRecoveryStep::Done;
            }
            UnauthorizedRecoveryStep::Done => {}
        }
        Ok(())
    }
}

/// Central manager providing a single source of truth for stored
/// authentication data. It loads once (or on preference change) and then
/// hands out cloned `CodexAuth` values so the rest of the program has a
/// consistent snapshot.
///
/// External modifications to stored credentials will NOT be observed until
/// `reload()` is called explicitly. This matches the design goal of avoiding
/// different parts of the program seeing inconsistent auth data mid‑run.
pub struct AuthManager {
    codex_home: PathBuf,
    inner: RwLock<CachedAuth>,
    auth_change_tx: watch::Sender<u64>,
    auth_change_state_tx: watch::Sender<AuthChangeState>,
    workspace_routing_resolver: OnceLock<Weak<dyn WorkspaceRoutingResolver>>,
    enable_codex_api_key_env: bool,
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    forced_login_method: Option<ForcedLoginMethod>,
    forced_chatgpt_workspace_id: RwLock<Option<Vec<String>>>,
    chatgpt_base_url: Option<String>,
    refresh_lock: Semaphore,
    auth_route_config: AuthRouteConfig,
}

/// Configuration view required to construct a shared [`AuthManager`].
///
/// Implementations should return the auth-related config values for the
/// already-resolved runtime configuration. The primary implementation is
/// `codex_core::config::Config`, but this trait keeps `codex-login` independent
/// from `codex-core`.
pub trait AuthManagerConfig {
    /// Returns the Codex home directory used for auth storage.
    fn codex_home(&self) -> PathBuf;

    /// Returns the CLI auth credential storage mode for auth loading.
    fn cli_auth_credentials_store_mode(&self) -> AuthCredentialsStoreMode;

    /// Returns the backend to use when CLI auth keyring storage is selected.
    fn auth_keyring_backend_kind(&self) -> AuthKeyringBackendKind;

    /// Returns the resolved login-method restriction, if any.
    fn forced_login_method(&self) -> Option<ForcedLoginMethod>;

    /// Returns the workspace IDs that ChatGPT auth should be restricted to, if any.
    fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>>;

    /// Returns the ChatGPT backend base URL used for first-party backend authorization.
    fn chatgpt_base_url(&self) -> String;

    /// Returns route-selection settings for auth-owned clients.
    fn auth_route_config(&self) -> AuthRouteConfig;
}

impl Debug for AuthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManager")
            .field("codex_home", &self.codex_home)
            .field("inner", &self.inner)
            .field("enable_codex_api_key_env", &self.enable_codex_api_key_env)
            .field(
                "auth_credentials_store_mode",
                &self.auth_credentials_store_mode,
            )
            .field("keyring_backend_kind", &self.keyring_backend_kind)
            .field("forced_login_method", &self.forced_login_method)
            .field(
                "forced_chatgpt_workspace_id",
                &self.forced_chatgpt_workspace_id,
            )
            .field("chatgpt_base_url", &self.chatgpt_base_url)
            .field("auth_route_config", &self.auth_route_config)
            .finish_non_exhaustive()
    }
}

impl AuthManager {
    /// Creates content clients using the application-resolved routing configuration.
    pub fn http_client_factory(&self) -> HttpClientFactory {
        self.auth_route_config.http_client_factory().clone()
    }
    /// Create a new manager loading the initial auth using the provided
    /// preferred auth method. Errors loading auth are swallowed; `auth()` will
    /// simply return `None` in that case so callers can treat it as an
    /// unauthenticated state.
    pub async fn new(
        codex_home: PathBuf,
        enable_codex_api_key_env: bool,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        chatgpt_base_url: Option<String>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: AuthRouteConfig,
    ) -> Self {
        Self::new_from_auth_config(
            AuthConfig {
                codex_home,
                auth_credentials_store_mode,
                keyring_backend_kind,
                forced_login_method: None,
                chatgpt_base_url,
                forced_chatgpt_workspace_id,
                auth_route_config,
            },
            enable_codex_api_key_env,
        )
        .await
    }

    async fn new_from_auth_config(auth_config: AuthConfig, enable_codex_api_key_env: bool) -> Self {
        let managed_auth = auth_config
            .load_auth(enable_codex_api_key_env)
            .ok()
            .flatten();
        let AuthConfig {
            codex_home,
            auth_credentials_store_mode,
            keyring_backend_kind,
            forced_login_method,
            chatgpt_base_url,
            forced_chatgpt_workspace_id,
            auth_route_config,
        } = auth_config;
        let (auth_change_tx, _auth_change_rx) = watch::channel(0);
        Self {
            codex_home,
            inner: RwLock::new(CachedAuth {
                auth: managed_auth,
                permanent_refresh_failure: None,
            }),
            auth_change_tx,
            auth_change_state_tx: watch::channel(AuthChangeState::default()).0,
            workspace_routing_resolver: OnceLock::new(),
            enable_codex_api_key_env,
            auth_credentials_store_mode,
            keyring_backend_kind,
            forced_login_method,
            forced_chatgpt_workspace_id: RwLock::new(forced_chatgpt_workspace_id),
            chatgpt_base_url,
            refresh_lock: Semaphore::new(/*permits*/ 1),
            auth_route_config,
        }
    }

    /// Current cached auth (clone) without attempting a refresh.
    pub fn auth_cached(&self) -> Option<CodexAuth> {
        self.inner
            .read()
            .ok()
            .and_then(|cached| cached.auth.clone())
    }

    /// Subscribes to cached auth changes that can affect request recovery.
    pub fn auth_change_receiver(&self) -> watch::Receiver<u64> {
        self.auth_change_tx.subscribe()
    }

    /// Subscribes to credential and owner revisions published together, including when changes coalesce.
    pub fn auth_change_state_receiver(&self) -> watch::Receiver<AuthChangeState> {
        self.auth_change_state_tx.subscribe()
    }

    pub fn refresh_failure_for_auth(&self, auth: &CodexAuth) -> Option<RefreshTokenFailedError> {
        self.inner.read().ok().and_then(|cached| {
            cached
                .permanent_refresh_failure
                .as_ref()
                .filter(|failure| Self::auths_equal_for_refresh(Some(auth), Some(&failure.auth)))
                .map(|failure| failure.error.clone())
        })
    }

    /// Current cached auth (clone). May be `None` if not logged in or load failed.
    /// For managed ChatGPT auth that needs a proactive refresh, first performs
    /// a guarded reload and then refreshes only if the on-disk auth is unchanged.
    #[instrument(level = "trace", skip_all)]
    pub async fn auth(&self) -> Option<CodexAuth> {
        let auth = self.auth_cached()?;
        if Self::should_refresh_proactively(&auth)
            && let Err(err) = self.refresh_token().await
        {
            tracing::error!("Failed to refresh token: {}", err);
            return Some(auth);
        }
        self.auth_cached()
    }

    /// Refreshes auth, then captures credentials and their account-bound factory together.
    /// The auth read lock prevents an identity change between the two snapshots.
    pub async fn auth_with_http_client_factory(&self) -> Option<(CodexAuth, HttpClientFactory)> {
        self.auth().await;
        let cached = self.inner.read().ok()?;
        Some((cached.auth.clone()?, self.http_client_factory()))
    }

    /// Reloads auth from the active source. Returns whether the auth value changed.
    pub async fn reload(&self) -> bool {
        tracing::info!("Reloading auth");
        let new_auth = self.load_auth();
        self.set_cached_auth(new_auth)
    }

    async fn reload_if_account_id_matches(
        &self,
        expected_account_id: Option<&str>,
    ) -> ReloadOutcome {
        let expected_account_id = match expected_account_id {
            Some(account_id) => account_id,
            None => {
                tracing::info!("Skipping auth reload because no account id is available.");
                return ReloadOutcome::Skipped;
            }
        };

        let new_auth = self.load_auth();
        let new_account_id = new_auth.as_ref().and_then(CodexAuth::get_account_id);

        if new_account_id.as_deref() != Some(expected_account_id) {
            let found_account_id = new_account_id.as_deref().unwrap_or("unknown");
            tracing::info!(
                "Skipping auth reload due to account id mismatch (expected: {expected_account_id}, found: {found_account_id})"
            );
            return ReloadOutcome::Skipped;
        }

        tracing::info!("Reloading auth for account {expected_account_id}");
        let cached_before_reload = self.auth_cached();
        let auth_changed =
            !Self::auths_equal_for_refresh(cached_before_reload.as_ref(), new_auth.as_ref());
        self.set_cached_auth(new_auth);
        if auth_changed {
            ReloadOutcome::ReloadedChanged
        } else {
            ReloadOutcome::ReloadedNoChange
        }
    }

    fn auths_equal_for_refresh(a: Option<&CodexAuth>, b: Option<&CodexAuth>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => match (a.auth_mode(), b.auth_mode()) {
                (AuthMode::ApiKey, AuthMode::ApiKey) => a.api_key() == b.api_key(),
                (AuthMode::Chatgpt, AuthMode::Chatgpt) => {
                    a.get_current_auth_json() == b.get_current_auth_json()
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn auths_equal(a: Option<&CodexAuth>, b: Option<&CodexAuth>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Records a permanent refresh failure only if the failed refresh was
    /// attempted against the auth snapshot that is still cached.
    fn record_permanent_refresh_failure_if_unchanged(
        &self,
        attempted_auth: &CodexAuth,
        error: &RefreshTokenFailedError,
    ) {
        if let Ok(mut guard) = self.inner.write() {
            let current_auth_matches =
                Self::auths_equal_for_refresh(Some(attempted_auth), guard.auth.as_ref());
            if current_auth_matches {
                guard.permanent_refresh_failure = Some(AuthScopedRefreshFailure {
                    auth: attempted_auth.clone(),
                    error: error.clone(),
                });
            }
        }
    }

    fn load_auth(&self) -> Option<CodexAuth> {
        let allowed_login_methods = self.allowed_login_methods();
        load_auth(
            &self.codex_home,
            self.enable_codex_api_key_env,
            self.auth_credentials_store_mode,
            Some(&allowed_login_methods),
            self.keyring_backend_kind,
            &self.auth_route_config,
        )
        .ok()
        .flatten()
        .filter(|auth| {
            validate_auth_restrictions(
                Some(&allowed_login_methods),
                self.effective_chatgpt_workspaces().as_deref(),
                auth,
            )
            .is_ok()
        })
    }

    fn set_cached_auth(&self, new_auth: Option<CodexAuth>) -> bool {
        if let Ok(mut guard) = self.inner.write() {
            let previous = guard.auth.as_ref();
            let changed = !AuthManager::auths_equal(previous, new_auth.as_ref());
            let auth_changed_for_refresh =
                !Self::auths_equal_for_refresh(previous, new_auth.as_ref());
            let owner_changed =
                auth_changed_for_refresh && !same_owner(previous, new_auth.as_ref());
            if auth_changed_for_refresh {
                guard.permanent_refresh_failure = None;
            }
            tracing::info!("Reloaded auth, changed: {changed}");
            guard.auth = new_auth;
            if auth_changed_for_refresh {
                self.auth_change_state_tx.send_modify(|state| {
                    state.generation += 1;
                    if owner_changed {
                        state.owner_generation += 1;
                    }
                });
                self.auth_change_tx.send_modify(|revision| *revision += 1);
            }
            changed
        } else {
            false
        }
    }

    pub fn set_forced_chatgpt_workspace_id(&self, workspace_id: Option<Vec<String>>) {
        if let Ok(mut guard) = self.forced_chatgpt_workspace_id.write()
            && *guard != workspace_id
        {
            *guard = workspace_id;
        }
    }

    pub fn forced_chatgpt_workspace_id(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub fn effective_chatgpt_workspaces(&self) -> Option<Vec<String>> {
        self.forced_chatgpt_workspace_id()
    }

    pub fn is_login_method_allowed(&self, method: ForcedLoginMethod) -> bool {
        login_method_allowed(
            method,
            self.forced_login_method,
            self.forced_chatgpt_workspace_id().as_deref(),
        )
    }

    /// Returns the login methods permitted by the configured login restrictions.
    pub fn allowed_login_methods(&self) -> Vec<ForcedLoginMethod> {
        allowed_login_methods(
            self.forced_login_method,
            self.forced_chatgpt_workspace_id().as_deref(),
        )
    }

    pub fn codex_api_key_env_enabled(&self) -> bool {
        self.enable_codex_api_key_env
    }

    /// Convenience constructor returning an `Arc` wrapper.
    pub async fn shared(
        codex_home: PathBuf,
        enable_codex_api_key_env: bool,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        forced_chatgpt_workspace_id: Option<Vec<String>>,
        chatgpt_base_url: Option<String>,
        keyring_backend_kind: AuthKeyringBackendKind,
        auth_route_config: AuthRouteConfig,
    ) -> Arc<Self> {
        Arc::new(
            Self::new(
                codex_home,
                enable_codex_api_key_env,
                auth_credentials_store_mode,
                forced_chatgpt_workspace_id,
                chatgpt_base_url,
                keyring_backend_kind,
                auth_route_config,
            )
            .await,
        )
    }

    /// Builds a shared manager from resolved runtime configuration.
    pub async fn shared_from_config(
        config: &impl AuthManagerConfig,
        enable_codex_api_key_env: bool,
    ) -> Arc<Self> {
        Arc::new(
            Self::new_from_auth_config(auth_config_from(config), enable_codex_api_key_env).await,
        )
    }

    pub fn unauthorized_recovery(self: &Arc<Self>) -> UnauthorizedRecovery {
        UnauthorizedRecovery::new(Arc::clone(self))
    }

    /// Attempt to refresh the token by first performing a guarded reload from
    /// the active auth source. If the loaded token differs from the cached token,
    /// we can assume that the source already refreshed it. Otherwise, ask the
    /// token authority to refresh.
    pub async fn refresh_token(&self) -> Result<(), RefreshTokenError> {
        let _refresh_guard = self.refresh_lock.acquire().await.map_err(|_| {
            RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
            ))
        })?;
        let auth_before_reload = self.auth_cached();
        if auth_before_reload
            .as_ref()
            .is_some_and(CodexAuth::is_api_key_auth)
        {
            return Ok(());
        }
        let expected_account_id = auth_before_reload
            .as_ref()
            .and_then(CodexAuth::get_account_id);

        match self
            .reload_if_account_id_matches(expected_account_id.as_deref())
            .await
        {
            ReloadOutcome::ReloadedChanged => {
                tracing::info!("Skipping token refresh because auth changed after guarded reload.");
                Ok(())
            }
            ReloadOutcome::ReloadedNoChange => self.refresh_token_from_authority_impl().await,
            ReloadOutcome::Skipped => {
                Err(RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                    RefreshTokenFailedReason::Other,
                    REFRESH_TOKEN_ACCOUNT_MISMATCH_MESSAGE.to_string(),
                )))
            }
        }
    }

    /// Attempt to refresh the current auth token from the authority that issued
    /// it and update the shared cache. If the token refresh fails, returns the
    /// error to the caller.
    pub async fn refresh_token_from_authority(&self) -> Result<(), RefreshTokenError> {
        let _refresh_guard = self.refresh_lock.acquire().await.map_err(|_| {
            RefreshTokenError::Permanent(RefreshTokenFailedError::new(
                RefreshTokenFailedReason::Other,
                REFRESH_TOKEN_UNKNOWN_MESSAGE.to_string(),
            ))
        })?;
        self.refresh_token_from_authority_impl().await
    }

    async fn refresh_token_from_authority_impl(&self) -> Result<(), RefreshTokenError> {
        tracing::info!("Refreshing token");

        let attempted_auth = self.auth_cached();
        if let Some(error) = attempted_auth
            .as_ref()
            .and_then(|auth| self.refresh_failure_for_auth(auth))
        {
            return Err(RefreshTokenError::Permanent(error));
        }

        let result = match attempted_auth.as_ref() {
            Some(CodexAuth::Chatgpt(chatgpt_auth)) => {
                let token_data = chatgpt_auth.current_token_data().ok_or_else(|| {
                    RefreshTokenError::Transient(std::io::Error::other(
                        "Token data is not available.",
                    ))
                })?;
                self.refresh_and_persist_chatgpt_token(chatgpt_auth, token_data.refresh_token)
                    .await
            }
            Some(CodexAuth::ApiKey(_)) | None => Ok(()),
        };
        if let Some(attempted_auth) = attempted_auth.as_ref()
            && let Err(RefreshTokenError::Permanent(error)) = &result
        {
            self.record_permanent_refresh_failure_if_unchanged(attempted_auth, error);
        }
        result
    }

    /// Log out by deleting credentials from the selected storage backend and the
    /// ephemeral store. Returns `Ok(true)` if credentials were removed, `Ok(false)`
    /// if none were present. On success, reloads the in-memory auth cache so
    /// callers immediately observe the unauthenticated state.
    pub async fn logout(&self) -> std::io::Result<bool> {
        let removed = logout_all_stores(
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )?;
        // Always reload to clear cached auth, even if no stored credentials were present.
        self.reload().await;
        Ok(removed)
    }

    pub async fn logout_with_revoke(&self) -> std::io::Result<bool> {
        let auth_dot_json = self
            .auth_cached()
            .and_then(|auth| auth.get_current_auth_json());
        if let Err(err) = revoke_auth_tokens(auth_dot_json.as_ref(), &self.auth_route_config).await
        {
            tracing::warn!("failed to revoke auth tokens during logout: {err}");
        }
        let result = logout_all_stores(
            &self.codex_home,
            self.auth_credentials_store_mode,
            self.keyring_backend_kind,
        )?;
        // Always reload to clear cached auth, even if no stored credentials were present.
        self.reload().await;
        Ok(result)
    }

    /// Returns the kind of credentials backing the current authentication.
    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth_cached().as_ref().map(CodexAuth::auth_mode)
    }

    pub fn current_auth_uses_codex_backend(&self) -> bool {
        self.auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::uses_codex_backend)
    }

    fn should_refresh_proactively(auth: &CodexAuth) -> bool {
        let chatgpt_auth = match auth {
            CodexAuth::Chatgpt(chatgpt_auth) => chatgpt_auth,
            _ => return false,
        };

        let auth_dot_json = match chatgpt_auth.current_auth_json() {
            Some(auth_dot_json) => auth_dot_json,
            None => return false,
        };
        if let Some(tokens) = auth_dot_json.tokens.as_ref()
            && let Ok(Some(expires_at)) = parse_jwt_expiration(&tokens.access_token)
        {
            return expires_at
                <= Utc::now()
                    + chrono::Duration::minutes(CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES);
        }
        let last_refresh = match auth_dot_json.last_refresh {
            Some(last_refresh) => last_refresh,
            None => return false,
        };
        last_refresh < Utc::now() - chrono::Duration::days(TOKEN_REFRESH_INTERVAL)
    }

    // Refreshes ChatGPT OAuth tokens, persists the updated auth state, and
    // reloads the in-memory cache so callers immediately observe new tokens.
    async fn refresh_and_persist_chatgpt_token(
        &self,
        auth: &ChatgptAuth,
        refresh_token: String,
    ) -> Result<(), RefreshTokenError> {
        let refresh_response = request_chatgpt_token_refresh(refresh_token, auth.client()).await?;

        persist_tokens(
            auth.storage(),
            refresh_response.id_token,
            refresh_response.access_token,
            refresh_response.refresh_token,
        )
        .map_err(RefreshTokenError::from)?;
        self.reload().await;

        Ok(())
    }
}

fn auth_config_from(config: &impl AuthManagerConfig) -> AuthConfig {
    AuthConfig {
        codex_home: config.codex_home(),
        auth_credentials_store_mode: config.cli_auth_credentials_store_mode(),
        keyring_backend_kind: config.auth_keyring_backend_kind(),
        forced_login_method: config.forced_login_method(),
        chatgpt_base_url: Some(config.chatgpt_base_url()),
        forced_chatgpt_workspace_id: config.forced_chatgpt_workspace_id(),
        auth_route_config: config.auth_route_config(),
    }
}

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_agent_identity::AgentIdentityKey;
use codex_agent_identity::authorization_header_for_agent_task;
use codex_api::AgentIdentityTelemetry;
use codex_api::AuthError;
use codex_api::AuthHeadersFuture;
use codex_api::AuthProvider;
use codex_api::SharedAuthProvider;
use codex_login::AuthHeaders;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuth;
use codex_login::auth::AgentIdentityAuthError;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::SessionSource;
use http::HeaderMap;
use http::HeaderValue;

use crate::bearer_auth_provider::BearerAuthProvider;

const BEDROCK_API_KEY_UNSUPPORTED_MESSAGE: &str =
    "Bedrock API key auth is only supported by the Amazon Bedrock model provider";

#[derive(Clone, Debug)]
pub struct ProviderAuthScope {
    pub agent_identity_policy: AgentIdentityAuthPolicy,
    pub session_source: SessionSource,
    pub agent_identity_session_fallback: AgentIdentitySessionFallback,
}

#[derive(Clone, Debug, Default)]
pub struct AgentIdentitySessionFallback {
    engaged: Arc<AtomicBool>,
}

impl AgentIdentitySessionFallback {
    pub fn is_engaged(&self) -> bool {
        self.engaged.load(Ordering::Relaxed)
    }

    fn engage(&self) -> bool {
        !self.engaged.swap(true, Ordering::Relaxed)
    }
}

/// Provider auth resolved for a request, plus metadata describing the effective auth.
#[derive(Clone)]
pub struct ResolvedProviderAuth {
    pub auth: SharedAuthProvider,
    pub agent_identity_telemetry: Option<AgentIdentityTelemetry>,
}

impl ResolvedProviderAuth {
    pub(crate) fn new(auth: SharedAuthProvider) -> Self {
        Self {
            auth,
            agent_identity_telemetry: None,
        }
    }

    fn for_agent_identity(auth: AgentIdentityAuth) -> Self {
        let agent_identity_telemetry = agent_identity_telemetry(&auth);
        Self {
            auth: Arc::new(AgentIdentityAuthProvider { auth }),
            agent_identity_telemetry: Some(agent_identity_telemetry),
        }
    }
}

pub(crate) fn agent_identity_telemetry(auth: &AgentIdentityAuth) -> AgentIdentityTelemetry {
    AgentIdentityTelemetry {
        agent_id: auth.record().agent_runtime_id.clone(),
        task_id: auth.run_task_id().to_string(),
    }
}

#[derive(Clone, Debug)]
struct AgentIdentityAuthProvider {
    auth: AgentIdentityAuth,
}

impl AuthProvider for AgentIdentityAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let record = self.auth.record();
        let header_value = authorization_header_for_agent_task(
            AgentIdentityKey {
                agent_runtime_id: &record.agent_runtime_id,
                private_key_pkcs8_base64: &record.agent_private_key,
            },
            self.auth.run_task_id(),
        )
        .map_err(std::io::Error::other);

        if let Ok(header_value) = header_value
            && let Ok(header) = HeaderValue::from_str(&header_value)
        {
            let _ = headers.insert(http::header::AUTHORIZATION, header);
        }

        if let Ok(header) = HeaderValue::from_str(self.auth.account_id()) {
            let _ = headers.insert("ChatGPT-Account-ID", header);
        }

        if self.auth.is_fedramp_account() {
            let _ = headers.insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }
    }
}

#[derive(Clone, Debug)]
struct HeaderAuthProvider {
    auth: AuthHeaders,
}

impl AuthProvider for HeaderAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        headers.extend(self.auth.headers().clone());
    }
}

struct AuthManagerAuthProvider {
    auth_manager: Arc<AuthManager>,
    // Startup auth is only the account-scoped identity anchor. Request
    // headers always come from the current AuthManager snapshot below.
    expected_auth: CodexAuth,
}

impl AuthManagerAuthProvider {
    fn is_expected_auth(&self, auth: &CodexAuth) -> bool {
        auth.uses_codex_backend()
            && auth.get_account_id() == self.expected_auth.get_account_id()
            && auth.get_chatgpt_user_id() == self.expected_auth.get_chatgpt_user_id()
            && auth.is_workspace_account() == self.expected_auth.is_workspace_account()
    }

    fn current_auth(&self) -> Option<CodexAuth> {
        self.auth_manager
            .auth_cached()
            .filter(|auth| self.is_expected_auth(auth))
    }
}

impl AuthProvider for AuthManagerAuthProvider {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let Some(auth) = self.current_auth() else {
            return;
        };
        auth_provider_from_auth(&auth).add_auth_headers(headers);
    }

    fn resolve_auth_headers(&self) -> AuthHeadersFuture<'_> {
        Box::pin(async move {
            let auth = self
                .auth_manager
                .auth()
                .await
                .filter(|auth| self.is_expected_auth(auth))
                .ok_or_else(|| {
                    AuthError::Transient("managed authentication is unavailable".to_string())
                })?;
            Ok(auth_provider_from_auth(&auth).to_auth_headers())
        })
    }
}

// Some providers are meant to send no auth headers. Examples include local OSS
// providers and custom test providers with `requires_openai_auth = false`.
#[derive(Clone, Debug)]
struct UnauthenticatedAuthProvider;

impl AuthProvider for UnauthenticatedAuthProvider {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

pub fn unauthenticated_auth_provider() -> SharedAuthProvider {
    Arc::new(UnauthenticatedAuthProvider)
}

/// Returns the provider-scoped auth manager when this provider uses command-backed auth.
///
/// Providers without custom auth continue using the caller-supplied base manager, when present.
pub(crate) fn auth_manager_for_provider(
    auth_manager: Option<Arc<AuthManager>>,
    provider: &ModelProviderInfo,
) -> Option<Arc<AuthManager>> {
    match provider.auth.clone() {
        Some(config) => Some(AuthManager::external_bearer_only(config)),
        None => auth_manager,
    }
}

pub(crate) fn resolve_provider_auth(
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
) -> codex_protocol::error::Result<SharedAuthProvider> {
    if let Some(auth) = bearer_auth_for_provider(provider)? {
        return Ok(Arc::new(auth));
    }

    if !provider.requires_openai_auth && provider.auth.is_none() {
        return Ok(unauthenticated_auth_provider());
    }

    if matches!(
        auth,
        Some(CodexAuth::BedrockApiKey(_) | CodexAuth::BedrockAccessKeys(_))
    ) {
        return Err(CodexErr::UnsupportedOperation(
            BEDROCK_API_KEY_UNSUPPORTED_MESSAGE.to_string(),
        ));
    }

    Ok(match auth {
        Some(auth) => auth_provider_from_auth(auth),
        None => unauthenticated_auth_provider(),
    })
}

pub(crate) async fn resolve_provider_auth_for_scope(
    auth_manager: Option<Arc<AuthManager>>,
    auth: Option<&CodexAuth>,
    provider: &ModelProviderInfo,
    scope: ProviderAuthScope,
) -> codex_protocol::error::Result<ResolvedProviderAuth> {
    let ProviderAuthScope {
        agent_identity_policy,
        session_source,
        agent_identity_session_fallback,
    } = scope;
    if let Some(CodexAuth::AgentIdentity(agent_identity_auth)) = auth {
        return Ok(ResolvedProviderAuth::for_agent_identity(
            agent_identity_auth.clone(),
        ));
    }

    if !should_bootstrap_chatgpt_agent_identity(agent_identity_policy, auth)
        || agent_identity_session_fallback.is_engaged()
    {
        return resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new);
    }

    let Some(auth_manager) = auth_manager else {
        return resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new);
    };

    match auth_manager
        .agent_identity_auth(agent_identity_policy, session_source)
        .await
    {
        Ok(Some(agent_identity_auth)) => Ok(ResolvedProviderAuth::for_agent_identity(
            agent_identity_auth,
        )),
        Ok(None) => resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new),
        Err(err) => {
            if let Some(AgentIdentityAuthError::BootstrapUnavailable {
                operation,
                attempts,
                message,
            }) = err
                .get_ref()
                .and_then(|source| source.downcast_ref::<AgentIdentityAuthError>())
            {
                let newly_engaged = agent_identity_session_fallback.engage();
                tracing::warn!(
                    operation,
                    attempts = *attempts,
                    error = %message,
                    newly_engaged,
                    "agent identity bootstrap unavailable; using ChatGPT bearer auth for this session"
                );
                resolve_provider_auth(auth, provider).map(ResolvedProviderAuth::new)
            } else {
                Err(err.into())
            }
        }
    }
}

fn should_bootstrap_chatgpt_agent_identity(
    agent_identity_policy: AgentIdentityAuthPolicy,
    auth: Option<&CodexAuth>,
) -> bool {
    agent_identity_policy == AgentIdentityAuthPolicy::ChatGptAuth
        && matches!(auth, Some(CodexAuth::Chatgpt(_)))
}

fn bearer_auth_for_provider(
    provider: &ModelProviderInfo,
) -> codex_protocol::error::Result<Option<BearerAuthProvider>> {
    if let Some(api_key) = provider.api_key()? {
        return Ok(Some(BearerAuthProvider::new(api_key)));
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        return Ok(Some(BearerAuthProvider::new(token.into_inner())));
    }

    Ok(None)
}

/// Builds request-header auth for a first-party Codex auth snapshot.
pub fn auth_provider_from_auth(auth: &CodexAuth) -> SharedAuthProvider {
    match auth {
        CodexAuth::AgentIdentity(auth) => {
            Arc::new(AgentIdentityAuthProvider { auth: auth.clone() })
        }
        CodexAuth::Headers(auth) => Arc::new(HeaderAuthProvider { auth: auth.clone() }),
        CodexAuth::BedrockApiKey(_) | CodexAuth::BedrockAccessKeys(_) => {
            unreachable!("{BEDROCK_API_KEY_UNSUPPORTED_MESSAGE}")
        }
        CodexAuth::ApiKey(_)
        | CodexAuth::Chatgpt(_)
        | CodexAuth::ChatgptAuthTokens(_)
        | CodexAuth::PersonalAccessToken(_) => Arc::new(BearerAuthProvider {
            token: auth.get_token().ok(),
            account_id: auth.get_account_id(),
            is_fedramp_account: auth.is_fedramp_account(),
        }),
    }
}

/// Builds request-header auth that reads the current managed auth snapshot on
/// every request while remaining scoped to the expected auth identity.
///
/// Callers with account-scoped state should pass the same snapshot that keyed
/// that state so a later account switch cannot reuse it.
pub fn auth_provider_from_auth_manager(
    auth_manager: Arc<AuthManager>,
    expected_auth: &CodexAuth,
) -> SharedAuthProvider {
    Arc::new(AuthManagerAuthProvider {
        auth_manager,
        expected_auth: expected_auth.clone(),
    })
}

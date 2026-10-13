use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_api::TransportError;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::WorkspaceRoutingRequest;
use codex_login::default_client::ClientRedirectPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::cache::ModelsCache;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::account::ProviderAccount;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ModelsResponse;

use crate::ProviderCapabilities;
use crate::ResolvedResponsesProvider;
use crate::auth::resolve_provider_auth;
use crate::models_endpoint::OpenAiModelsEndpoint;
use crate::workspace_routing::WorkspaceRoutingContext;

/// Current app-visible account state for a model provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountState {
    pub account: Option<ProviderAccount>,
    pub requires_openai_auth: bool,
}

/// Outcome of a provider-owned attempt to recover from an authentication failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderUnauthorizedRecovery {
    /// The provider has no provider-specific authentication recovery configured.
    NotConfigured,
    /// The provider recovered its authentication state and the request can be retried.
    Recovered,
}

/// User-facing lifecycle messages for provider-owned authentication recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderAuthRecoveryMessages {
    pub started: &'static str,
    pub succeeded: &'static str,
}

/// Error returned when a provider cannot construct its app-visible account state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAccountError {
    MissingChatgptAccountDetails,
}

impl fmt::Display for ProviderAccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingChatgptAccountDetails => {
                write!(f, "plan type is required for chatgpt authentication")
            }
        }
    }
}

impl std::error::Error for ProviderAccountError {}

pub type ProviderAccountResult = std::result::Result<ProviderAccountState, ProviderAccountError>;

/// Default model used for automatic approval review when a provider does not
/// require a backend-specific model ID.
pub const DEFAULT_APPROVAL_REVIEW_PREFERRED_MODEL: &str = "codex-auto-review";

const API_KEY_APPROVAL_REVIEW_PREFERRED_MODEL: &str = "gpt-5.6-luna";

/// Runtime provider abstraction used by model execution.
///
/// Implementations own provider-specific behavior for a model backend. The
/// `ModelProviderInfo` returned by `info` is the serialized/configured provider
/// metadata used by the default OpenAI-compatible implementation.
pub trait ModelProvider: fmt::Debug + Send + Sync {
    /// Returns the configured provider metadata.
    fn info(&self) -> &ModelProviderInfo;

    /// Returns whether the resolved Responses provider may receive internal tool metadata.
    fn include_internal_metadata(&self, provider: &Provider) -> bool {
        self.info().include_internal_metadata
            || url::Url::parse(&provider.base_url).ok().is_some_and(|url| {
                url.scheme() == "https"
                    && url.host_str().is_some_and(|host| {
                        host == "api.openai.com" || codex_http_client::is_allowed_chatgpt_host(host)
                    })
            })
    }

    /// Returns the provider-owned capability upper bounds.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Returns the preferred model used for automatic approval review.
    ///
    /// Providers that require backend-specific model IDs should override this.
    fn approval_review_preferred_model(&self) -> &'static str {
        DEFAULT_APPROVAL_REVIEW_PREFERRED_MODEL
    }

    /// Returns whether requests made through this provider should include attestation.
    fn supports_attestation(&self) -> bool {
        false
    }

    /// Returns the provider-scoped auth manager, when this provider uses one.
    ///
    /// TODO(celia-oai): Make auth manager access internal to this crate so callers
    /// resolve provider-specific auth only through `ModelProvider`. We first need
    /// to think through whether Codex should have a unified provider-specific auth
    /// manager throughout the codebase; that is a larger refactor than this change.
    fn auth_manager(&self) -> Option<Arc<AuthManager>>;

    /// Returns whether this transport failure can be recovered by provider-scoped authentication.
    ///
    /// The default preserves existing unauthorized-response handling. Providers with other
    /// authentication failure shapes may recognize additional response or request-signing errors.
    fn is_recoverable_auth_error(&self, error: &TransportError) -> bool {
        matches!(
            error,
            TransportError::Http { status, .. } if *status == http::StatusCode::UNAUTHORIZED
        )
    }

    /// Returns lifecycle messages when provider-owned authentication recovery is active.
    fn auth_recovery_messages(&self) -> Option<ProviderAuthRecoveryMessages> {
        None
    }

    /// Attempts provider-owned authentication recovery before using the auth manager.
    fn recover_from_unauthorized(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<ProviderUnauthorizedRecovery>> {
        Box::pin(async { Ok(ProviderUnauthorizedRecovery::NotConfigured) })
    }

    /// Returns the current provider-scoped auth value, if one is configured.
    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>>;

    /// Returns the current app-visible account state for this provider.
    fn account_state(&self) -> ProviderAccountResult;

    /// Maps an API client error into the provider's user-facing error representation.
    fn map_api_error(&self, error: ApiError) -> CodexErr {
        codex_api::map_api_error(error)
    }

    /// Returns provider configuration adapted for the API client.
    fn api_provider(&self) -> ModelProviderFuture<'_, codex_protocol::error::Result<Provider>> {
        Box::pin(async move {
            let auth = self.auth().await;
            self.info()
                .to_api_provider(auth.as_ref().map(CodexAuth::auth_mode))
        })
    }

    /// Resolves routing for Responses HTTP, compaction, and WebSocket handshakes.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "serialize discovery and the session's first successful routing transition"
    )]
    fn responses_api_provider<'a>(
        &'a self,
        routing_context: &'a WorkspaceRoutingContext,
    ) -> ModelProviderFuture<'a, codex_protocol::error::Result<ResolvedResponsesProvider>> {
        Box::pin(async move {
            let mut provider = self.api_provider().await?;
            let mut redirect_policy = ClientRedirectPolicy::Default;
            if provider_uses_first_party_auth_path(self.info())
                && self.info().supports_codex_backend_routes()
                && let Some(auth) = self.auth().await.filter(CodexAuth::is_chatgpt_auth)
                && let Some(auth_manager) = self.auth_manager()
            {
                let mut previously_routed = routing_context.previously_routed.lock().await;
                if let Some(routing) = auth_manager
                    .workspace_routing(
                        &auth,
                        WorkspaceRoutingRequest {
                            provider_base_url: provider.base_url.clone(),
                            chatgpt_base_url: routing_context.chatgpt_base_url.clone(),
                            previously_routed: *previously_routed,
                            session: routing_context.session.clone(),
                        },
                    )
                    .await?
                {
                    crate::workspace_routing::apply_workspace_routing(&mut provider, routing)?;
                    redirect_policy = ClientRedirectPolicy::Reject;
                    *previously_routed = true;
                }
            }
            Ok(ResolvedResponsesProvider {
                provider,
                redirect_policy,
            })
        })
    }

    /// Returns the provider base URL that will be used at request time.
    fn runtime_base_url(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<Option<String>>> {
        Box::pin(async { Ok(self.info().base_url.clone()) })
    }

    /// Returns the auth provider used to attach request credentials.
    fn api_auth(
        &self,
    ) -> ModelProviderFuture<'_, codex_protocol::error::Result<SharedAuthProvider>> {
        Box::pin(async move {
            let auth = self.auth().await;
            resolve_provider_auth(auth.as_ref(), self.info())
        })
    }

    /// Creates the model manager implementation appropriate for this provider.
    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager;

    /// Creates a model manager with caching disabled.
    ///
    /// Providers that fetch model catalogs should override this method. The default uses an
    /// authoritative in-memory catalog so hosted callers cannot accidentally write to disk.
    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        let model_catalog = config_model_catalog
            .or_else(|| codex_models_manager::bundled_models_response().ok())
            .unwrap_or_default();
        Arc::new(StaticModelsManager::new(self.auth_manager(), model_catalog))
    }

    /// Creates a model manager that can use a caller-provided cache for remote catalogs.
    ///
    /// Providers with remote catalogs should override this method. The default preserves the
    /// authoritative catalog returned by [`ModelProvider::models_manager_without_cache`] and does
    /// not consult `cache`. Implementations should likewise ignore the cache when
    /// `config_model_catalog` supplies an authoritative static catalog.
    fn models_manager_with_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
        cache: Arc<dyn ModelsCache>,
    ) -> SharedModelsManager {
        drop(cache);
        self.models_manager_without_cache(config_model_catalog)
    }
}

pub type ModelProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shared runtime model provider handle.
pub type SharedModelProvider = Arc<dyn ModelProvider>;

fn provider_uses_first_party_auth_path(provider: &ModelProviderInfo) -> bool {
    provider.requires_openai_auth
        && provider.env_key.is_none()
        && provider.experimental_bearer_token.is_none()
}

/// Creates the default runtime model provider for configured provider metadata.
pub fn create_model_provider(
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
) -> SharedModelProvider {
    Arc::new(ConfiguredModelProvider::new(provider_info, auth_manager))
}

/// Runtime model provider for OpenAI and OpenAI-compatible endpoints.
#[derive(Clone, Debug)]
struct ConfiguredModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
}

enum ModelsCacheConfig {
    Disk { codex_home: PathBuf },
    Disabled,
    Custom(Arc<dyn ModelsCache>),
}

impl ConfiguredModelProvider {
    fn new(info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        Self { info, auth_manager }
    }

    fn create_models_manager(
        &self,
        config_model_catalog: Option<ModelsResponse>,
        cache: ModelsCacheConfig,
    ) -> SharedModelsManager {
        if let Some(model_catalog) = config_model_catalog {
            return Arc::new(StaticModelsManager::new(
                self.auth_manager.clone(),
                model_catalog,
            ));
        }
        let endpoint = Arc::new(OpenAiModelsEndpoint::new(
            self.info.clone(),
            self.auth_manager.clone(),
        ));
        let auth_manager = self.auth_manager.clone();
        let manager = match cache {
            ModelsCacheConfig::Disk { codex_home } => {
                OpenAiModelsManager::new(codex_home, endpoint, auth_manager)
            }
            ModelsCacheConfig::Disabled => {
                OpenAiModelsManager::new_without_cache(endpoint, auth_manager)
            }
            ModelsCacheConfig::Custom(cache) => {
                OpenAiModelsManager::new_with_cache(cache, endpoint, auth_manager)
            }
        };
        match &self.info.model_catalog_url {
            Some(_) => Arc::new(manager.with_provider_catalog()),
            None => Arc::new(manager),
        }
    }
}

impl ModelProvider for ConfiguredModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::from_config(&self.info)
    }

    fn approval_review_preferred_model(&self) -> &'static str {
        if self
            .auth_manager
            .as_ref()
            .and_then(|auth_manager| auth_manager.auth_cached())
            .is_some_and(|auth| auth.is_api_key_auth())
        {
            API_KEY_APPROVAL_REVIEW_PREFERRED_MODEL
        } else {
            DEFAULT_APPROVAL_REVIEW_PREFERRED_MODEL
        }
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    fn supports_attestation(&self) -> bool {
        self.auth_manager
            .as_ref()
            .and_then(|auth_manager| auth_manager.auth_cached())
            .is_some_and(|auth| auth.is_chatgpt_auth())
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async move {
            match self.auth_manager.as_ref() {
                Some(auth_manager) => auth_manager.auth().await,
                None => None,
            }
        })
    }

    fn account_state(&self) -> ProviderAccountResult {
        let account = if self.info.requires_openai_auth {
            self.auth_manager
                .as_ref()
                .and_then(|auth_manager| {
                    let auth = auth_manager.auth_cached()?;
                    if auth_manager.refresh_failure_for_auth(&auth).is_some() {
                        return None;
                    }
                    Some(auth)
                })
                .map(|auth| match &auth {
                    CodexAuth::ApiKey(_) => Ok(ProviderAccount::ApiKey),
                    CodexAuth::Chatgpt(_) => {
                        let email = auth.get_account_email();
                        let plan_type = auth.account_plan_type();

                        plan_type
                            .map(|plan_type| ProviderAccount::Chatgpt { email, plan_type })
                            .ok_or(ProviderAccountError::MissingChatgptAccountDetails)
                    }
                })
                .transpose()?
        } else {
            None
        };

        Ok(ProviderAccountState {
            account,
            requires_openai_auth: self.info.requires_openai_auth,
        })
    }

    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.create_models_manager(config_model_catalog, ModelsCacheConfig::Disk { codex_home })
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        self.create_models_manager(config_model_catalog, ModelsCacheConfig::Disabled)
    }

    fn models_manager_with_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
        cache: Arc<dyn ModelsCache>,
    ) -> SharedModelsManager {
        self.create_models_manager(config_model_catalog, ModelsCacheConfig::Custom(cache))
    }
}

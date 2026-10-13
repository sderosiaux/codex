use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use codex_api::ModelsClient;
use codex_api::ReqwestTransport;
use codex_api::TransportError;
use codex_api::map_api_error;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::ClientRedirectPolicy;
use codex_login::default_client::create_client_for_route_async;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::manager::ModelsEndpointFuture;
use codex_models_manager::manager::ModelsEndpointResponse;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use http::HeaderMap;
use tokio::time::timeout;

use crate::auth::resolve_provider_auth;

const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
// Bound downloads from explicitly configured catalogs before decoding or caching them.
const MAX_MODEL_CATALOG_BYTES: usize = 1024 * 1024;

/// Provider-owned OpenAI-compatible `/models` endpoint.
#[derive(Debug)]
pub(crate) struct OpenAiModelsEndpoint {
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
    transport_builder: Arc<dyn ModelsTransportBuilder>,
}

impl OpenAiModelsEndpoint {
    pub(crate) fn new(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        let redirect_policy = if provider_info.model_catalog_url.is_some() {
            ClientRedirectPolicy::Reject
        } else {
            ClientRedirectPolicy::Default
        };
        Self {
            provider_info,
            auth_manager,
            transport_builder: Arc::new(RouteAwareModelsTransportBuilder { redirect_policy }),
        }
    }

    async fn auth(&self) -> Option<CodexAuth> {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager.auth().await,
            None => None,
        }
    }

    async fn uses_codex_backend(&self) -> bool {
        self.auth()
            .await
            .as_ref()
            .is_some_and(CodexAuth::uses_codex_backend)
    }

    async fn list_models(
        &self,
        client_version: &str,
        http_client_factory: HttpClientFactory,
    ) -> CoreResult<ModelsEndpointResponse> {
        let auth = self.auth().await;
        let identity = crate::models_identity::identity(&self.provider_info, auth.as_ref())?;
        let auth_mode = auth.as_ref().map(CodexAuth::auth_mode);
        let mut api_provider = self.provider_info.to_api_provider(auth_mode)?;
        if (auth.as_ref().is_some_and(CodexAuth::is_api_key_auth) || self.has_provider_api_key())
            && self.supports_api_key_models()
            && self.provider_info.base_url.is_none()
            && self.provider_info.model_catalog_url.is_none()
        {
            // Codex metadata is served by the Codex backend, not the public /v1/models API.
            api_provider.base_url = CHATGPT_CODEX_BASE_URL.to_string();
        }
        let api_auth = resolve_provider_auth(auth.as_ref(), &self.provider_info)?;
        let request_url = match self.provider_info.model_catalog_url.as_deref() {
            Some(catalog_url) => ModelsClient::<ReqwestTransport>::catalog_request_url(
                &api_provider,
                catalog_url,
                client_version,
            )
            .map_err(map_api_error)?,
            None => ModelsClient::<ReqwestTransport>::request_url(&api_provider, client_version),
        };
        let (models, etag) = timeout(MODELS_REFRESH_TIMEOUT, async {
            let transport = self
                .transport_builder
                .build(http_client_factory, request_url.clone())
                .await?;
            let client = ModelsClient::new(transport, api_provider, api_auth);
            let response_body_limit_bytes = self
                .provider_info
                .model_catalog_url
                .as_ref()
                .map(|_| MAX_MODEL_CATALOG_BYTES);
            client
                .list_models(request_url, HeaderMap::new(), response_body_limit_bytes)
                .await
                .map_err(|mut error| {
                    if self.provider_info.model_catalog_url.is_some()
                        && let codex_api::ApiError::Transport(TransportError::Http {
                            url,
                            headers,
                            body,
                            ..
                        }) = &mut error
                    {
                        // Provider diagnostics may echo URL credentials or other secrets.
                        *url = None;
                        *headers = None;
                        *body = None;
                    }
                    map_api_error(error)
                })
        })
        .await
        .map_err(|_| CodexErr::RequestTimeout)??;
        if self.provider_info.model_catalog_url.is_some() {
            let mut slugs = HashSet::new();
            if models
                .iter()
                .any(|model| model.slug.trim().is_empty() || !slugs.insert(&model.slug))
            {
                return Err(CodexErr::InvalidRequest(
                    "model catalog must contain unique, non-empty model slugs".to_string(),
                ));
            }
        }
        Ok(ModelsEndpointResponse {
            models,
            etag,
            identity,
        })
    }
}

impl ModelsEndpointClient for OpenAiModelsEndpoint {
    fn supports_api_key_models(&self) -> bool {
        self.provider_info.model_catalog_url.is_some()
            || (self.provider_info.is_openai() && self.provider_info.base_url.is_none())
    }

    fn has_provider_api_key(&self) -> bool {
        self.provider_info.env_key.is_some()
            || self.provider_info.experimental_bearer_token.is_some()
    }

    fn identity(&self) -> Option<String> {
        let auth = self
            .auth_manager
            .as_ref()
            .and_then(|manager| manager.auth_cached());
        crate::models_identity::identity(&self.provider_info, auth.as_ref()).ok()
    }

    fn uses_codex_backend(&self) -> ModelsEndpointFuture<'_, bool> {
        Box::pin(OpenAiModelsEndpoint::uses_codex_backend(self))
    }

    fn list_models<'a>(
        &'a self,
        client_version: &'a str,
        http_client_factory: HttpClientFactory,
    ) -> ModelsEndpointFuture<'a, CoreResult<ModelsEndpointResponse>> {
        Box::pin(OpenAiModelsEndpoint::list_models(
            self,
            client_version,
            http_client_factory,
        ))
    }
}

type ModelsTransportFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<ReqwestTransport>> + Send + 'a>>;

/// Builds the concrete transport selected for one models request.
///
/// Implementations must honor the supplied request-time client factory and exact request URL.
trait ModelsTransportBuilder: fmt::Debug + Send + Sync {
    fn build(
        &self,
        http_client_factory: HttpClientFactory,
        request_url: String,
    ) -> ModelsTransportFuture<'_>;
}

#[derive(Debug)]
struct RouteAwareModelsTransportBuilder {
    redirect_policy: ClientRedirectPolicy,
}

impl ModelsTransportBuilder for RouteAwareModelsTransportBuilder {
    fn build(
        &self,
        http_client_factory: HttpClientFactory,
        request_url: String,
    ) -> ModelsTransportFuture<'_> {
        let redirect_policy = self.redirect_policy;
        Box::pin(async move {
            let client = create_client_for_route_async(
                http_client_factory,
                request_url,
                ClientRouteClass::Api,
                redirect_policy,
            )
            .await?;
            let client = match redirect_policy {
                ClientRedirectPolicy::Default => client,
                ClientRedirectPolicy::Reject => client.without_request_logging(),
            };
            Ok(ReqwestTransport::from_http_client(client))
        })
    }
}

use crate::auth::SharedAuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use bytes::Bytes;
use codex_client::HttpTransport;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use http::HeaderMap;
use http::Method;
use http::header::ETAG;
use url::Url;

pub struct ModelsClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> ModelsClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    fn path() -> &'static str {
        "models"
    }

    fn append_client_version_query(req: &mut codex_client::Request, client_version: &str) {
        let separator = if req.url.contains('?') { '&' } else { '?' };
        req.url = format!("{}{}client_version={client_version}", req.url, separator);
    }

    pub fn request_url(provider: &Provider, client_version: &str) -> String {
        let mut request = provider.build_request(Method::GET, Self::path());
        Self::append_client_version_query(&mut request, client_version);
        request.url
    }

    /// Builds a full catalog URL, preserving provider routing parameters and client version.
    pub fn catalog_request_url(
        provider: &Provider,
        catalog_url: &str,
        client_version: &str,
    ) -> Result<String, ApiError> {
        let invalid = |message: &str| ApiError::InvalidRequest {
            message: message.to_string(),
        };
        let mut url = Url::parse(catalog_url)
            .map_err(|_| invalid("model_catalog_url must be an absolute URL"))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(params) = &provider.query_params {
                query.extend_pairs(params);
            }
            query.append_pair("client_version", client_version);
        }
        Ok(url.into())
    }

    /// Fetches and decodes a catalog, optionally bounding response bytes before decoding.
    pub async fn list_models(
        &self,
        request_url: String,
        extra_headers: HeaderMap,
        response_body_limit_bytes: Option<usize>,
    ) -> Result<(Vec<ModelInfo>, Option<String>), ApiError> {
        let (body, header_etag) = self
            .list_models_raw(request_url, extra_headers, response_body_limit_bytes)
            .await?;
        let ModelsResponse { models } =
            serde_json::from_slice::<ModelsResponse>(&body).map_err(|e| {
                ApiError::Stream(format!(
                    "failed to decode models response: {:?} at line {} column {} (body: {} bytes)",
                    e.classify(),
                    e.line(),
                    e.column(),
                    body.len()
                ))
            })?;

        Ok((models, header_etag))
    }

    /// Fetches a catalog using the provider's auth and retry policy without decoding it.
    ///
    /// Callers accepting provider-controlled catalogs must set a response-body limit
    /// and validate the native response without including raw values in diagnostics.
    pub async fn list_models_raw(
        &self,
        request_url: String,
        extra_headers: HeaderMap,
        response_body_limit_bytes: Option<usize>,
    ) -> Result<(Bytes, Option<String>), ApiError> {
        let resp = self
            .session
            .execute_with(
                Method::GET,
                Self::path(),
                extra_headers,
                /*body*/ None,
                move |req| {
                    req.url.clone_from(&request_url);
                    req.response_body_limit_bytes = response_body_limit_bytes;
                },
            )
            .await?;

        let header_etag = resp
            .headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);

        Ok((resp.body, header_etag))
    }
}

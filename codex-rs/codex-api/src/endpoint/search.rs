use crate::auth::SharedAuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::search::SearchRequest;
use crate::search::SearchResponse;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use http::HeaderMap;
use http::Method;
use serde_json::to_value;
use std::sync::Arc;

/// The provider-relative endpoint for standalone web search.
const SEARCH_ENDPOINT: &str = "alpha/search";

pub struct SearchClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> SearchClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    pub fn with_telemetry(self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
        }
    }

    pub async fn search(
        &self,
        request: &SearchRequest,
        extra_headers: HeaderMap,
    ) -> Result<SearchResponse, ApiError> {
        let body = to_value(request)
            .map_err(|e| ApiError::Stream(format!("failed to encode search request: {e}")))?;
        let resp = self
            .session
            .execute(Method::POST, SEARCH_ENDPOINT, extra_headers, Some(body))
            .await?;
        serde_json::from_slice(&resp.body)
            .map_err(|e| ApiError::Stream(format!("failed to decode search response: {e}")))
    }
}

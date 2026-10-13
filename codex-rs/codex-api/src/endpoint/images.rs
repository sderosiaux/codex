use crate::auth::SharedAuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::images::ImageEditRequest;
use crate::images::ImageGenerationRequest;
use crate::images::ImageResponse;
use crate::provider::Provider;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
use serde::Serialize;
use serde_json::to_value;
use std::sync::Arc;

const X_CODEX_IMAGEGEN_REQUEST_ID_HEADER: &str = "x-codex-imagegen-request-id";

pub struct ImagesClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

/// Image request failure with the nested ImageGen request ID, when available.
#[derive(Debug)]
pub struct ImageRequestError {
    error: ApiError,
    imagegen_request_id: Option<String>,
}

impl ImageRequestError {
    fn from_api_error(error: ApiError) -> Self {
        let imagegen_request_id = match &error {
            ApiError::Transport(TransportError::Http { headers, .. }) => {
                headers.as_ref().and_then(imagegen_request_id_from_headers)
            }
            _ => None,
        };
        Self {
            error,
            imagegen_request_id,
        }
    }

    pub fn into_parts(self) -> (ApiError, Option<String>) {
        (self.error, self.imagegen_request_id)
    }
}

impl<T: HttpTransport> ImagesClient<T> {
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

    pub async fn generate(
        &self,
        request: &ImageGenerationRequest,
        extra_headers: HeaderMap,
    ) -> Result<(ImageResponse, Option<String>), ImageRequestError> {
        self.post_image_request(
            "images/generations",
            request,
            extra_headers,
            "image generation",
        )
        .await
    }

    pub async fn edit(
        &self,
        request: &ImageEditRequest,
        extra_headers: HeaderMap,
    ) -> Result<(ImageResponse, Option<String>), ImageRequestError> {
        self.post_image_request("images/edits", request, extra_headers, "image edit")
            .await
    }

    async fn post_image_request<R: Serialize>(
        &self,
        path: &str,
        request: &R,
        extra_headers: HeaderMap,
        operation: &str,
    ) -> Result<(ImageResponse, Option<String>), ImageRequestError> {
        let body = to_value(request).map_err(|e| ImageRequestError {
            error: ApiError::Stream(format!("failed to encode {operation} request: {e}")),
            imagegen_request_id: None,
        })?;
        let resp = self
            .session
            .execute(Method::POST, path, extra_headers, Some(body))
            .await
            .map_err(ImageRequestError::from_api_error)?;
        let imagegen_request_id = imagegen_request_id_from_headers(&resp.headers);
        let response = serde_json::from_slice(&resp.body).map_err(|e| ImageRequestError {
            error: ApiError::Stream(format!("failed to decode {operation} response: {e}")),
            imagegen_request_id: imagegen_request_id.clone(),
        })?;
        Ok((response, imagegen_request_id))
    }
}

fn imagegen_request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(X_CODEX_IMAGEGEN_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_string)
}

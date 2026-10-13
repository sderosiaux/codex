//! Local HTTP capability: requests go out directly from this process through the
//! shared route-aware transport.

use std::pin::Pin;
use std::time::Duration;

use crate::rpc::HandlerError;
use bytes::Bytes;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::HttpError;
use codex_http_client::HttpResponse;
use codex_http_client::RouteAwareClientPool;
use codex_http_client::RouteAwareRequestError;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use url::Url;

use crate::ExecServerError;
use crate::protocol::HttpHeader;
use crate::protocol::HttpRedirectPolicy;
use crate::protocol::HttpRequestParams;
use crate::protocol::HttpRequestResponse;
use crate::rpc::internal_error;
use crate::rpc::invalid_params;

/// Sends HTTP requests on behalf of MCP clients and extensions.
pub trait HttpClient: Send + Sync {
    /// Perform an HTTP request and buffer the response body.
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>>;

    /// Perform an HTTP request and return a streamed body handle.
    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>>;
}

/// Request-scoped stream of body chunks for an HTTP response.
pub struct HttpResponseBodyStream {
    body: Pin<Box<dyn futures::Stream<Item = Result<Bytes, HttpError>> + Send>>,
    cancellation: Option<CancellationToken>,
}

impl HttpResponseBodyStream {
    /// Creates an in-memory response stream from pre-buffered chunks.
    pub fn from_chunks(chunks: Vec<Vec<u8>>) -> Self {
        let body = futures::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, HttpError>(chunk.into())),
        );
        Self {
            body: Box::pin(body),
            cancellation: None,
        }
    }

    fn local(response: HttpResponse) -> Self {
        Self {
            body: Box::pin(response.bytes_stream()),
            cancellation: None,
        }
    }

    /// Ends this stream when its owning request authority is revoked.
    pub fn cancel_on(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Receives the next response-body chunk; `Ok(None)` at EOF.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, ExecServerError> {
        let Some(cancellation) = self.cancellation.clone() else {
            return self.recv_inner().await;
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Ok(None),
            result = self.recv_inner() => result,
        }
    }

    async fn recv_inner(&mut self) -> Result<Option<Vec<u8>>, ExecServerError> {
        match self.body.next().await {
            Some(Ok(bytes)) => Ok(Some(bytes.to_vec())),
            Some(Err(error)) => Err(ExecServerError::HttpRequest(error.to_string())),
            None => Ok(None),
        }
    }
}

/// HTTP capability backed by the shared route-aware transport.
#[derive(Clone)]
pub struct RouteAwareHttpClient {
    follow_redirects: RouteAwareClientPool,
    stop_redirects: RouteAwareClientPool,
}

struct RouteAwareHttpRequestRunner {
    client: RouteAwareClientPool,
}

impl RouteAwareHttpClient {
    pub fn new(http_client_factory: HttpClientFactory) -> Self {
        Self {
            follow_redirects: RouteAwareClientPool::with_chatgpt_cloudflare_cookies_without_request_logging(
                http_client_factory.clone(),
                ClientRouteClass::Other,
            ),
            stop_redirects:
                RouteAwareClientPool::with_chatgpt_cloudflare_cookies_without_redirects_or_request_logging(
                    http_client_factory,
                    ClientRouteClass::Other,
                ),
        }
    }

    /// Enables narrowly scoped TLS-backend fallback for both redirect policies.
    pub fn with_tls_backend_fallback(mut self) -> Self {
        self.follow_redirects = self.follow_redirects.with_tls_backend_fallback();
        self.stop_redirects = self.stop_redirects.with_tls_backend_fallback();
        self
    }

    fn runner(&self, redirect_policy: HttpRedirectPolicy) -> RouteAwareHttpRequestRunner {
        let client = match redirect_policy {
            HttpRedirectPolicy::Follow => self.follow_redirects.clone(),
            HttpRedirectPolicy::Stop => self.stop_redirects.clone(),
        };
        RouteAwareHttpRequestRunner { client }
    }
}

impl HttpClient for RouteAwareHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let runner = self.runner(params.redirect_policy);
            let (response, _) = runner
                .run(HttpRequestParams {
                    stream_response: false,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            Ok(response)
        }
        .boxed()
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async move {
            let runner = self.runner(params.redirect_policy);
            let (response, body) = runner
                .run(HttpRequestParams {
                    stream_response: true,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let body = body.ok_or_else(|| {
                ExecServerError::Protocol(
                    "http request stream did not return a response body stream".to_string(),
                )
            })?;
            Ok((response, HttpResponseBodyStream::local(body)))
        }
        .boxed()
    }
}

impl RouteAwareHttpRequestRunner {
    async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<HttpResponse>), HandlerError> {
        let method = Method::from_bytes(params.method.as_bytes())
            .map_err(|error| invalid_params(format!("http/request method is invalid: {error}")))?;
        let url = Url::parse(&params.url)
            .map_err(|error| invalid_params(format!("http/request url is invalid: {error}")))?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(invalid_params(format!(
                    "http/request only supports http and https URLs, got {scheme}"
                )));
            }
        }

        let request_span = tracing::info_span!(
            "codex.exec_server.http_request",
            http.request.method = method.as_str(),
            server.address = url.host_str().unwrap_or_default(),
            server.port = u64::from(url.port_or_known_default().unwrap_or_default()),
            http.response.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let headers = Self::build_headers(params.headers)?;
        let mut request = self.client.request(method.clone(), url).headers(headers);
        if let Some(body) = params.body {
            request = request.body(body.into_inner());
        }
        if let Some(timeout_ms) = params.timeout_ms {
            request = request.timeout(Duration::from_millis(timeout_ms));
        }

        let response = match request.send().instrument(request_span.clone()).await {
            Ok(response) => response,
            Err(error) => {
                request_span.record("error.type", "request");
                let error_message = error.to_string();
                log_send_error(&method, error);
                return Err(internal_error(format!(
                    "http/request failed: {error_message}"
                )));
            }
        };
        let status = response.status().as_u16();
        request_span.record("http.response.status_code", u64::from(status));
        let headers = Self::response_headers(response.headers());

        if params.stream_response {
            return Ok((
                HttpRequestResponse {
                    status,
                    headers,
                    body: Vec::new().into(),
                },
                Some(response),
            ));
        }

        let body = response.bytes().await.map_err(|error| {
            internal_error(format!(
                "failed to read http/request response body: {error}"
            ))
        })?;

        Ok((
            HttpRequestResponse {
                status,
                headers,
                body: body.to_vec().into(),
            },
            None,
        ))
    }

    fn build_headers(headers: Vec<HttpHeader>) -> Result<HeaderMap, HandlerError> {
        let mut header_map = HeaderMap::new();
        for header in headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
                invalid_params(format!("http/request header name is invalid: {error}"))
            })?;
            let value = match header.value_env_var {
                Some(env_var) => {
                    let env_value = std::env::var(&env_var).map_err(|_| {
                        invalid_params(format!(
                            "http/request header {} requires executor environment variable {env_var}",
                            header.name
                        ))
                    })?;
                    if env_value.is_empty() {
                        return Err(invalid_params(format!(
                            "http/request header {} requires a non-empty executor environment variable {env_var}",
                            header.name
                        )));
                    }
                    format!("{}{env_value}", header.value)
                }
                None => header.value,
            };
            let value = HeaderValue::from_str(&value).map_err(|error| {
                invalid_params(format!(
                    "http/request header value is invalid for {}: {error}",
                    header.name
                ))
            })?;
            header_map.append(name, value);
        }
        Ok(header_map)
    }

    fn response_headers(headers: &HeaderMap) -> Vec<HttpHeader> {
        headers
            .iter()
            .filter_map(|(name, value)| {
                Some(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().ok()?.to_string(),
                    value_env_var: None,
                })
            })
            .collect()
    }
}

fn log_send_error(method: &Method, error: RouteAwareRequestError) {
    let error_is_timeout = error.is_timeout();
    let error_is_connect = error.is_connect();
    let error = match error {
        RouteAwareRequestError::Request(error) => error.without_url().to_string(),
        error => error.to_string(),
    };
    tracing::warn!(
        http_method = method.as_str(),
        error_is_timeout,
        error_is_connect,
        error = %error,
        "http/request send failed"
    );
}

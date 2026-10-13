use std::future::Future;
use std::time::Duration;

use crate::AuthProvider;
use bytes::Bytes;
use codex_http_client::HttpResponse;
use codex_http_client::RequestBuilder;
use codex_http_client::RouteAwareClientPool;
use codex_http_client::RouteAwareRequestError;
use futures::Stream;
use http::Method;
use http::StatusCode;
use http::header::CONTENT_LENGTH;
use serde::Deserialize;
use tokio::time::Instant;
use uuid::Uuid;

pub const OPENAI_FILE_URI_PREFIX: &str = "sediment://";
pub const OPENAI_FILE_UPLOAD_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

const OPENAI_FILE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const OPENAI_FILE_BLOB_UPLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_BLOB_UPLOAD_ATTEMPTS: u64 = 5;
const OPENAI_FILE_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const OPENAI_FILE_FINALIZE_RETRY_DELAY: Duration = Duration::from_millis(250);
const OPENAI_FILE_USE_CASE: &str = "codex";

#[derive(Debug)]
pub struct HostedFileUploadContext {
    pub connector_id: String,
    pub action_name: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedOpenAiFile {
    pub file_id: String,
    pub uri: String,
    pub download_url: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub mime_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenAiFileError {
    #[error("failed to open OpenAI file upload contents: {0}")]
    ReadContents(#[source] std::io::Error),
    #[error(
        "file `{file_name}` is too large: {size_bytes} bytes exceeds the limit of {limit_bytes} bytes"
    )]
    FileTooLarge {
        file_name: String,
        size_bytes: u64,
        limit_bytes: u64,
    },
    #[error("failed to send OpenAI file request to {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: RouteAwareRequestError,
    },
    #[error(
        "OpenAI file blob upload to {host} failed after {elapsed_ms} ms ({error_kind}, azure_client_request_id={azure_client_request_id}): {source}"
    )]
    BlobUploadRequest {
        host: String,
        elapsed_ms: u128,
        error_kind: &'static str,
        azure_client_request_id: String,
        #[source]
        source: RouteAwareRequestError,
    },
    #[error(
        "OpenAI file blob upload to {host} failed with status {status} (azure_client_request_id={azure_client_request_id}, azure_request_id={azure_request_id}, azure_error_code={azure_error_code})"
    )]
    BlobUploadStatus {
        host: String,
        status: StatusCode,
        azure_client_request_id: String,
        azure_request_id: String,
        azure_error_code: String,
    },
    #[error("OpenAI file request to {url} failed with status {status}: {body}")]
    UnexpectedStatus {
        url: String,
        status: StatusCode,
        body: String,
    },
    #[error("failed to parse OpenAI file response from {url}: {source}")]
    Decode {
        url: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("OpenAI file upload for `{file_id}` is not ready yet")]
    UploadNotReady { file_id: String },
    #[error("OpenAI file upload for `{file_id}` failed: {message}")]
    UploadFailed { file_id: String, message: String },
}

#[derive(Deserialize)]
struct CreateFileResponse {
    file_id: String,
    upload_url: String,
    #[serde(default)]
    pdf_c2pa_reservation: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct DownloadLinkResponse {
    status: String,
    download_url: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    error_message: Option<String>,
    #[serde(default)]
    file_size_bytes: Option<u64>,
}

pub fn openai_file_uri(file_id: &str) -> String {
    format!("{OPENAI_FILE_URI_PREFIX}{file_id}")
}

/// Creates and finalizes one file record, reopening the same contents for blob PUT retries.
pub async fn upload_openai_file<Open, OpenFuture, Contents>(
    base_url: &str,
    auth: &dyn AuthProvider,
    client_pool: &RouteAwareClientPool,
    file_name: String,
    file_size_bytes: u64,
    mut open_contents: Open,
    hosted_upload: Option<&HostedFileUploadContext>,
) -> Result<UploadedOpenAiFile, OpenAiFileError>
where
    Open: FnMut() -> OpenFuture,
    OpenFuture: Future<Output = std::io::Result<Contents>>,
    Contents: Stream<Item = std::io::Result<Bytes>> + Send + 'static,
{
    if file_size_bytes > OPENAI_FILE_UPLOAD_LIMIT_BYTES {
        return Err(OpenAiFileError::FileTooLarge {
            file_name,
            size_bytes: file_size_bytes,
            limit_bytes: OPENAI_FILE_UPLOAD_LIMIT_BYTES,
        });
    }

    let create_url = format!("{}/files", base_url.trim_end_matches('/'));
    let create_request = serde_json::json!({
        "file_name": file_name.as_str(),
        "file_size": file_size_bytes,
        "use_case": OPENAI_FILE_USE_CASE,
    });
    let request = authorized_request(client_pool, auth, Method::POST, &create_url);
    let create_request = match hosted_upload {
        Some(context) => serde_json::json!({
            "file_name": file_name.as_str(),
            "file_size": file_size_bytes,
            "use_case": OPENAI_FILE_USE_CASE,
            "codex_connector_id": context.connector_id,
            "codex_action_name": context.action_name,
            "codex_model": context.model,
        }),
        None => create_request,
    };
    let create_response = request
        .json(&create_request)
        .send()
        .await
        .map_err(|source| OpenAiFileError::Request {
            url: create_url.clone(),
            source,
        })?;
    let create_status = create_response.status();
    let create_body = create_response
        .text()
        .await
        .map_err(|source| OpenAiFileError::Request {
            url: create_url.clone(),
            source,
        })?;
    if !create_status.is_success() {
        return Err(OpenAiFileError::UnexpectedStatus {
            url: create_url,
            status: create_status,
            body: create_body,
        });
    }
    let create_payload: CreateFileResponse =
        serde_json::from_str(&create_body).map_err(|source| OpenAiFileError::Decode {
            url: create_url.clone(),
            source,
        })?;

    let upload_host = url::Url::parse(&create_payload.upload_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown-host".to_string());
    let upload_started_at = Instant::now();
    let deadline = upload_started_at + OPENAI_FILE_BLOB_UPLOAD_TIMEOUT;
    let mut retry_delay = Duration::ZERO;
    for attempt in 1..=MAX_BLOB_UPLOAD_ATTEMPTS {
        let azure_client_request_id = Uuid::new_v4().to_string();
        let request_error = |source: RouteAwareRequestError| {
            let error_kind = if source.is_timeout() {
                "timeout"
            } else if source.is_connect() {
                "connect"
            } else if source.is_body() {
                "body"
            } else if source.is_request() {
                "request"
            } else {
                "other"
            };
            OpenAiFileError::BlobUploadRequest {
                host: upload_host.clone(),
                elapsed_ms: upload_started_at.elapsed().as_millis(),
                error_kind,
                azure_client_request_id: azure_client_request_id.clone(),
                source: source.without_url(),
            }
        };
        // Opening the stream, sending every attempt, and backoff share one deadline.
        let result = tokio::time::timeout_at(deadline, async {
            if Instant::now() >= deadline {
                return Err(request_error(RouteAwareRequestError::Timeout));
            }
            let contents = open_contents()
                .await
                .map_err(OpenAiFileError::ReadContents)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(request_error(RouteAwareRequestError::Timeout));
            }
            client_pool
                .put(&create_payload.upload_url)
                .timeout(remaining)
                .header("x-ms-blob-type", "BlockBlob")
                .header("x-ms-client-request-id", &azure_client_request_id)
                .header(CONTENT_LENGTH, file_size_bytes)
                .body_stream(contents)
                .send()
                .await
                .map_err(request_error)
        })
        .await
        .unwrap_or_else(|_| Err(request_error(RouteAwareRequestError::Timeout)));

        let mut status = None;
        let mut cloudflare_ray_id = "missing".to_string();
        let mut azure_request_id = "missing".to_string();
        let mut azure_error_code = "missing".to_string();
        let mut server_delay = None;
        let (error, retryable, error_kind) = match result {
            Ok(response) => {
                let upload_status = response.status();
                status = Some(upload_status.to_string());
                cloudflare_ray_id = upload_response_header(&response, "cf-ray");
                azure_request_id = upload_response_header(&response, "x-ms-request-id");
                azure_error_code = upload_response_header(&response, "x-ms-error-code");
                if upload_status.is_success() {
                    if attempt > 1 {
                        tracing::event!(
                            target: "codex_otel.log_only",
                            tracing::Level::INFO,
                            event.name = "codex.openai_file_blob_upload_recovered",
                            file_id = %create_payload.file_id,
                            host = %upload_host,
                            file_size_bytes,
                            elapsed_ms = upload_started_at.elapsed().as_millis(),
                            attempt,
                            delay_ms = retry_delay.as_millis(),
                            status = status.as_deref(),
                            cloudflare_ray_id,
                            azure_client_request_id,
                            azure_request_id,
                            azure_error_code,
                            "OpenAI file blob upload recovered"
                        );
                    }
                    break;
                }
                server_delay = blob_retry_after(response.headers());
                (
                    OpenAiFileError::BlobUploadStatus {
                        host: upload_host.clone(),
                        status: upload_status,
                        azure_client_request_id: azure_client_request_id.clone(),
                        azure_request_id: azure_request_id.clone(),
                        azure_error_code: azure_error_code.clone(),
                    },
                    matches!(
                        upload_status,
                        StatusCode::BAD_GATEWAY
                            | StatusCode::SERVICE_UNAVAILABLE
                            | StatusCode::GATEWAY_TIMEOUT
                    ),
                    "http_status",
                )
            }
            Err(error) => {
                let (retryable, error_kind) = match &error {
                    OpenAiFileError::BlobUploadRequest {
                        source, error_kind, ..
                    } => (
                        source.is_timeout()
                            || source.is_connect()
                            || source.is_body()
                            || source.is_request(),
                        *error_kind,
                    ),
                    _ => (false, "read"),
                };
                (error, retryable, error_kind)
            }
        };
        retry_delay = server_delay
            .unwrap_or_else(|| codex_client::backoff(Duration::from_millis(125), attempt));
        let will_retry = retryable
            && attempt < MAX_BLOB_UPLOAD_ATTEMPTS
            && retry_delay < deadline.saturating_duration_since(Instant::now());
        let event_name = if will_retry {
            "codex.openai_file_blob_upload_retry"
        } else {
            "codex.openai_file_blob_upload_failed"
        };
        tracing::event!(
            target: "codex_otel.log_only",
            tracing::Level::WARN,
            event.name = event_name,
            file_id = %create_payload.file_id,
            host = %upload_host,
            file_size_bytes,
            elapsed_ms = upload_started_at.elapsed().as_millis(),
            attempt,
            delay_ms = if will_retry { retry_delay.as_millis() } else { 0 },
            status = status.as_deref(),
            error_kind,
            cloudflare_ray_id,
            azure_client_request_id,
            azure_request_id,
            azure_error_code,
            "OpenAI file blob upload attempt failed"
        );
        if !will_retry {
            return Err(error);
        }
        tokio::time::sleep(retry_delay).await;
    }

    let finalize_url = format!(
        "{}/files/{}/uploaded",
        base_url.trim_end_matches('/'),
        create_payload.file_id,
    );
    let finalize_request = serde_json::json!({});
    let finalize_request = if create_payload.pdf_c2pa_reservation {
        serde_json::json!({"pdf_c2pa_create_request": create_request})
    } else {
        finalize_request
    };
    let finalize_started_at = Instant::now();
    loop {
        let finalize_response = authorized_request(client_pool, auth, Method::POST, &finalize_url)
            .json(&finalize_request)
            .send()
            .await
            .map_err(|source| OpenAiFileError::Request {
                url: finalize_url.clone(),
                source,
            })?;
        let finalize_status = finalize_response.status();
        let finalize_body =
            finalize_response
                .text()
                .await
                .map_err(|source| OpenAiFileError::Request {
                    url: finalize_url.clone(),
                    source,
                })?;
        if !finalize_status.is_success() {
            return Err(OpenAiFileError::UnexpectedStatus {
                url: finalize_url.clone(),
                status: finalize_status,
                body: finalize_body,
            });
        }
        let finalize_payload: DownloadLinkResponse =
            serde_json::from_str(&finalize_body).map_err(|source| OpenAiFileError::Decode {
                url: finalize_url.clone(),
                source,
            })?;

        match finalize_payload.status.as_str() {
            "success" => {
                let file_size_bytes = finalize_payload.file_size_bytes.unwrap_or(file_size_bytes);
                return Ok(UploadedOpenAiFile {
                    file_id: create_payload.file_id.clone(),
                    uri: openai_file_uri(&create_payload.file_id),
                    download_url: finalize_payload.download_url.ok_or_else(|| {
                        OpenAiFileError::UploadFailed {
                            file_id: create_payload.file_id.clone(),
                            message: "missing download_url".to_string(),
                        }
                    })?,
                    file_name: finalize_payload.file_name.unwrap_or(file_name),
                    file_size_bytes,
                    mime_type: finalize_payload.mime_type,
                });
            }
            "retry" => {
                if finalize_started_at.elapsed() >= OPENAI_FILE_FINALIZE_TIMEOUT {
                    return Err(OpenAiFileError::UploadNotReady {
                        file_id: create_payload.file_id,
                    });
                }
                tokio::time::sleep(OPENAI_FILE_FINALIZE_RETRY_DELAY).await;
            }
            _ => {
                return Err(OpenAiFileError::UploadFailed {
                    file_id: create_payload.file_id,
                    message: finalize_payload
                        .error_message
                        .unwrap_or_else(|| "upload finalization returned an error".to_string()),
                });
            }
        }
    }
}

fn authorized_request(
    client_pool: &RouteAwareClientPool,
    auth: &dyn AuthProvider,
    method: Method,
    url: &str,
) -> RequestBuilder {
    let mut headers = http::HeaderMap::new();
    auth.add_auth_headers(&mut headers);

    client_pool
        .request(method, url)
        .timeout(OPENAI_FILE_REQUEST_TIMEOUT)
        .headers(headers)
}

fn upload_response_header(response: &HttpResponse, header: &str) -> String {
    response
        .headers()
        .get(header)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing")
        .to_string()
}

fn blob_retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    if let Some(delay) = headers
        .get("x-ms-retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
    {
        return Some(Duration::from_millis(delay));
    }
    let value = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = value.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        date.signed_duration_since(chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

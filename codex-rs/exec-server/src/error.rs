/// Errors returned by local command execution, filesystem and HTTP capabilities.
#[derive(Debug, thiserror::Error)]
pub enum ExecServerError {
    #[error("HTTP request failed: {0}")]
    HttpRequest(String),
    #[error("authentication required: {0}")]
    AuthenticationRequired(String),
    #[error("{0}")]
    Protocol(String),
    #[error("request rejected ({code}): {message}")]
    Server { code: i64, message: String },
}

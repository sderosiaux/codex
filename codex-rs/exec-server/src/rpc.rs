//! JSON-RPC style error values shared by the local process, shell snapshot and HTTP code paths.

/// Error returned by local process, filesystem, and HTTP handlers.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HandlerError {
    pub(crate) code: i64,
    pub(crate) data: Option<serde_json::Value>,
    pub(crate) message: String,
}

pub(crate) fn invalid_request(message: String) -> HandlerError {
    HandlerError {
        code: -32600,
        data: None,
        message,
    }
}

pub(crate) fn invalid_params(message: String) -> HandlerError {
    HandlerError {
        code: -32602,
        data: None,
        message,
    }
}

pub(crate) fn internal_error(message: String) -> HandlerError {
    HandlerError {
        code: -32603,
        data: None,
        message,
    }
}

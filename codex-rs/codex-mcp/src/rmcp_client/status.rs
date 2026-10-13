//! Observe the latest startup attempt without polling it or initiating a retry.

use codex_protocol::mcp::McpServerConnectionStatus as Status;

use super::AsyncManagedClient;
use super::StartupOutcomeError;

impl AsyncManagedClient {
    pub(crate) async fn connection_status(&self) -> Status {
        if self.cancel_token.is_cancelled() {
            return Status::Cancelled;
        }
        match self.client.peek().cloned() {
            Some(Ok(client)) => {
                if client.client.is_closed().await {
                    Status::Failed
                } else {
                    Status::Connected
                }
            }
            Some(Err(error)) if error.is_authentication_required() => {
                Status::AuthenticationRequired
            }
            Some(Err(StartupOutcomeError::Failed { .. })) => Status::Failed,
            Some(Err(StartupOutcomeError::Cancelled)) => Status::Cancelled,
            None => Status::Starting,
        }
    }
}

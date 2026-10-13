//! Reopens an agent only when its runtime is absent, then returns its effective state.
//! Its history comes from the in-process thread store; descendants are not reopened.

use super::LocalAgentControl;
use crate::agent::types::LiveAgent;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::SessionSource;

impl LocalAgentControl {
    pub(crate) async fn resume_agent(
        &self,
        config: Config,
        thread_id: ThreadId,
        source: SessionSource,
    ) -> CodexResult<(LiveAgent, ThreadConfigSnapshot)> {
        let manager = self.runtime.upgrade()?;
        let thread = match manager.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(err) if matches!(err.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                Box::pin(self.reopen_agent(config, thread_id, source)).await?;
                manager.get_thread(thread_id).await?
            }
            Err(err) => return Err(err),
        };
        let agent = LiveAgent {
            thread_id,
            metadata: self.get_agent_metadata(thread_id).unwrap_or_default(),
            status: thread.agent_status().await,
        };
        let config = thread.config_snapshot().await;
        Ok((agent, config))
    }
}

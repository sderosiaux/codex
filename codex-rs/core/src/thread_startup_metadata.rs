//! Retained startup metadata for new and resumed threads.

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::ThreadSource;
use codex_utils_absolute_path::AbsolutePathBuf;

/// The configuration reported when a thread started.
/// Current settings are available through [`crate::CodexThread::config_snapshot`].
#[derive(Debug)]
pub struct ThreadStartupMetadata {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    forked_from_id: Option<ThreadId>,
    parent_thread_id: Option<ThreadId>,
    thread_source: Option<ThreadSource>,
    model: String,
    model_provider_id: String,
    service_tier: Option<String>,
    cwd: AbsolutePathBuf,
    reasoning_effort: Option<ReasoningEffort>,
}

impl From<&SessionConfiguredEvent> for ThreadStartupMetadata {
    fn from(event: &SessionConfiguredEvent) -> Self {
        let SessionConfiguredEvent {
            session_id,
            thread_id,
            forked_from_id,
            parent_thread_id,
            thread_source,
            model,
            model_provider_id,
            service_tier,
            cwd,
            reasoning_effort,
        } = event;
        Self {
            session_id: *session_id,
            thread_id: *thread_id,
            forked_from_id: *forked_from_id,
            parent_thread_id: *parent_thread_id,
            thread_source: thread_source.clone(),
            model: model.clone(),
            model_provider_id: model_provider_id.clone(),
            service_tier: service_tier.clone(),
            cwd: cwd.clone(),
            reasoning_effort: reasoning_effort.clone(),
        }
    }
}

impl ThreadStartupMetadata {
    pub(crate) fn to_session_configured_event(&self) -> SessionConfiguredEvent {
        SessionConfiguredEvent {
            session_id: self.session_id,
            thread_id: self.thread_id,
            forked_from_id: self.forked_from_id,
            parent_thread_id: self.parent_thread_id,
            thread_source: self.thread_source.clone(),
            model: self.model.clone(),
            model_provider_id: self.model_provider_id.clone(),
            service_tier: self.service_tier.clone(),
            cwd: self.cwd.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

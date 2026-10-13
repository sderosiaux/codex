//! Owns a spawned child until its initial input is accepted.

use super::AgentTreeMembership;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrKind;
use std::sync::Arc;
use tracing::warn;

pub(super) struct PendingSpawn {
    state: Arc<ThreadManagerState>,
    pending: Option<(ThreadId, AgentTreeMembership)>,
}

impl PendingSpawn {
    pub(super) fn new(
        state: Arc<ThreadManagerState>,
        child: ThreadId,
        membership: AgentTreeMembership,
    ) -> Self {
        Self {
            state,
            pending: Some((child, membership)),
        }
    }

    pub(super) fn disarm(mut self) -> AgentTreeMembership {
        let Some((_child, membership)) = self.pending.take() else {
            unreachable!("pending spawn must own agent-tree membership");
        };
        membership
    }
}

impl Drop for PendingSpawn {
    fn drop(&mut self) {
        let Some((child, membership)) = self.pending.take() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let teardown = membership.into_teardown_guard("child_spawn_cleanup", Some(child));
        drop(tokio::spawn(async move {
            if let Some(thread) = state.remove_thread(&child).await {
                if let Err(error) = thread.shutdown_and_wait().await {
                    teardown
                        .record_shutdown_failure("stop_child", CodexErrKind::from(&error).into());
                    warn!("failed to stop cancelled child spawn: {error}");
                }
                if let Some(live_thread) = thread.session.live_thread() {
                    live_thread.discard().await;
                }
            }
            teardown.complete();
        }));
    }
}

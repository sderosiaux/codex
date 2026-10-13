use codex_protocol::ThreadId;
use codex_state::StateRuntime;
use std::sync::Arc;

use crate::AgentGraphStore;
use crate::AgentGraphStoreError;
use crate::AgentGraphStoreFuture;
use crate::ThreadSpawnEdgeStatus;

/// SQLite-backed implementation of [`AgentGraphStore`] using an existing state runtime.
#[derive(Clone)]
pub struct LocalAgentGraphStore {
    state_db: Arc<StateRuntime>,
}

impl std::fmt::Debug for LocalAgentGraphStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAgentGraphStore")
            .field("sqlite", self.state_db.sqlite())
            .finish_non_exhaustive()
    }
}

impl LocalAgentGraphStore {
    /// Create a local graph store from an already-initialized state runtime.
    pub fn new(state_db: Arc<StateRuntime>) -> Self {
        Self { state_db }
    }
}

impl AgentGraphStore for LocalAgentGraphStore {
    fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .upsert_thread_spawn_edge(
                    parent_thread_id,
                    child_thread_id,
                    to_state_status(status),
                )
                .await
                .map_err(internal_error)
        })
    }

    fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: ThreadSpawnEdgeStatus,
    ) -> AgentGraphStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state_db
                .set_thread_spawn_edge_status(child_thread_id, to_state_status(status))
                .await
                .map_err(internal_error)
        })
    }

    fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            if let Some(status) = status_filter {
                return self
                    .state_db
                    .list_thread_spawn_children_with_status(
                        parent_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(internal_error);
            }

            self.state_db
                .list_thread_spawn_children(parent_thread_id)
                .await
                .map_err(internal_error)
        })
    }

    fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
        status_filter: Option<ThreadSpawnEdgeStatus>,
    ) -> AgentGraphStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            match status_filter {
                Some(status) => self
                    .state_db
                    .list_thread_spawn_descendants_with_status(
                        root_thread_id,
                        to_state_status(status),
                    )
                    .await
                    .map_err(internal_error),
                None => self
                    .state_db
                    .list_thread_spawn_descendants(root_thread_id)
                    .await
                    .map_err(internal_error),
            }
        })
    }
}

fn to_state_status(status: ThreadSpawnEdgeStatus) -> codex_state::DirectionalThreadSpawnEdgeStatus {
    match status {
        ThreadSpawnEdgeStatus::Open => codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        ThreadSpawnEdgeStatus::Closed => codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
    }
}

fn internal_error(err: impl std::fmt::Display) -> AgentGraphStoreError {
    AgentGraphStoreError::Internal {
        message: err.to_string(),
    }
}

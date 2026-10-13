use std::future::Future;
use std::sync::Arc;

use codex_history::RolloutItem;
use codex_protocol::ThreadId;

use crate::CreateThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::policy::persisted_rollout_items;

/// Handle on an active thread's history in the [`ThreadStore`].
#[derive(Clone)]
pub struct LiveThread {
    thread_id: ThreadId,
    thread_store: Arc<ThreadStore>,
}

/// Owns a live thread while session initialization is still fallible.
///
/// Dropping the guard before [`commit`](Self::commit) forgets the thread, so a failed start does
/// not leave a half-registered history behind.
#[derive(Default)]
pub struct LiveThreadInitGuard {
    live_thread: Option<LiveThread>,
}

impl LiveThreadInitGuard {
    pub fn new(live_thread: Option<LiveThread>) -> Self {
        Self { live_thread }
    }

    pub async fn acquire(
        &mut self,
        acquisition: impl Future<Output = ThreadStoreResult<LiveThread>> + Send + 'static,
    ) -> ThreadStoreResult<LiveThread> {
        let live_thread = acquisition.await?;
        self.live_thread = Some(live_thread.clone());
        Ok(live_thread)
    }

    pub fn as_ref(&self) -> Option<&LiveThread> {
        self.live_thread.as_ref()
    }

    pub fn commit(&mut self) {
        self.live_thread = None;
    }

    pub async fn discard(&mut self) {
        if let Some(live_thread) = self.live_thread.take() {
            live_thread.discard().await;
        }
    }
}

impl Drop for LiveThreadInitGuard {
    fn drop(&mut self) {
        let Some(live_thread) = self.live_thread.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { live_thread.discard().await });
        }
    }
}

impl LiveThread {
    pub async fn create(
        thread_store: Arc<ThreadStore>,
        params: CreateThreadParams,
    ) -> ThreadStoreResult<Self> {
        let thread_id = params.thread_id;
        thread_store.create_thread(params).await?;
        Ok(Self {
            thread_id,
            thread_store,
        })
    }

    /// Creates a child thread whose history starts with the inherited model context.
    pub async fn create_with_inherited_model_context(
        thread_store: Arc<ThreadStore>,
        mut params: CreateThreadParams,
        inherited_model_context: &[RolloutItem],
        guard: &mut LiveThreadInitGuard,
    ) -> ThreadStoreResult<Self> {
        let prefix_item_count =
            persisted_rollout_items(inherited_model_context, params.history_mode).len();
        params.subagent_history_start_ordinal = Some(
            u64::try_from(prefix_item_count)
                .ok()
                .and_then(|count| count.checked_add(1))
                .ok_or_else(|| ThreadStoreError::Internal {
                    message: "inherited model context is too large".to_string(),
                })?,
        );
        let live_thread = guard.acquire(Self::create(thread_store, params)).await?;
        live_thread.append_items(inherited_model_context).await?;
        Ok(live_thread)
    }

    /// Reopens a thread and returns its replay history.
    pub async fn resume(
        thread_store: Arc<ThreadStore>,
        params: ResumeThreadParams,
    ) -> ThreadStoreResult<(Self, Arc<Vec<RolloutItem>>)> {
        let thread_id = params.thread_id;
        let history = thread_store.resume_thread(params).await?;
        Ok((
            Self {
                thread_id,
                thread_store,
            },
            history,
        ))
    }

    pub async fn append_items(&self, items: &[RolloutItem]) -> ThreadStoreResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.thread_store
            .append_items(self.thread_id, items.to_vec())
            .await
    }

    pub async fn discard(&self) {
        self.thread_store.discard_thread(self.thread_id).await;
    }

    pub async fn load_history(&self) -> ThreadStoreResult<StoredThreadHistory> {
        self.thread_store.load_history(self.thread_id).await
    }

    pub async fn read_thread(&self, include_history: bool) -> ThreadStoreResult<StoredThread> {
        self.thread_store
            .read_thread(self.thread_id, include_history)
            .await
    }
}

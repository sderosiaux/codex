use std::collections::HashMap;
use std::sync::Arc;

use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::Mutex;

use crate::CreateThreadParams;
use crate::ResumeThreadParams;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::policy::into_persisted_rollout_items;

/// In-memory registry of every thread's replayable history for the life of the process.
#[derive(Default)]
pub struct ThreadStore {
    threads: Mutex<HashMap<ThreadId, StoredEntry>>,
}

struct StoredEntry {
    created: Option<CreateThreadParams>,
    history_mode: ThreadHistoryMode,
    items: Vec<RolloutItem>,
}

impl ThreadStore {
    /// Registers a new thread whose history starts with its session metadata.
    pub async fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        let session_meta = SessionMeta {
            session_id: params.session_id,
            id: params.thread_id,
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            cwd: params.cwd.clone(),
            runtime_workspace_roots: params
                .runtime_workspace_roots
                .as_ref()
                .map(|roots| roots.iter().map(AbsolutePathBuf::to_path_buf).collect()),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            originator: params.originator.clone(),
            source: params.source.clone(),
            thread_source: params.thread_source.clone(),
            model_provider: Some(params.model_provider.clone()),
            base_instructions: Some(params.base_instructions.clone()),
            dynamic_tools: (!params.dynamic_tools.is_empty()).then(|| params.dynamic_tools.clone()),
            history_mode: params.history_mode,
            subagent_history_start_ordinal: params.subagent_history_start_ordinal,
            multi_agent_version: params.multi_agent_version,
            context_window: Some(SessionContextWindow::new(params.initial_window_id.clone())),
            ..SessionMeta::default()
        };
        let mut threads = self.threads.lock().await;
        let entry = threads
            .entry(params.thread_id)
            .or_insert_with(|| StoredEntry {
                created: None,
                history_mode: params.history_mode,
                items: Vec::new(),
            });
        entry.items.push(RolloutItem::SessionMeta(SessionMetaLine {
            meta: session_meta,
            git: None,
        }));
        entry.history_mode = params.history_mode;
        entry.created = Some(params);
        Ok(())
    }

    /// Returns the thread's history, installing the caller's history if the store has none.
    pub async fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreResult<Arc<Vec<RolloutItem>>> {
        let mut threads = self.threads.lock().await;
        let entry = match threads.entry(params.thread_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let items = params.history.map(Arc::unwrap_or_clone).ok_or(
                    ThreadStoreError::ThreadNotFound {
                        thread_id: params.thread_id,
                    },
                )?;
                entry.insert(StoredEntry {
                    created: None,
                    history_mode: history_mode_from_items(&items),
                    items,
                })
            }
        };
        Ok(Arc::new(entry.items.clone()))
    }

    /// Appends the replayable subset of `items` to the thread's history.
    pub async fn append_items(
        &self,
        thread_id: ThreadId,
        items: Vec<RolloutItem>,
    ) -> ThreadStoreResult<()> {
        let mut threads = self.threads.lock().await;
        let entry = threads
            .get_mut(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        let items = into_persisted_rollout_items(items, entry.history_mode);
        entry.items.extend(items);
        Ok(())
    }

    pub async fn load_history(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        let threads = self.threads.lock().await;
        let entry = threads
            .get(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        Ok(StoredThreadHistory {
            thread_id,
            items: entry.items.clone(),
        })
    }

    pub async fn read_thread(
        &self,
        thread_id: ThreadId,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        let threads = self.threads.lock().await;
        let entry = threads
            .get(&thread_id)
            .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
        let meta = entry.items.iter().find_map(|item| match item {
            RolloutItem::SessionMeta(line) if line.meta.id == thread_id => Some(&line.meta),
            _ => None,
        });
        let latest_turn = entry.items.iter().rev().find_map(|item| match item {
            RolloutItem::TurnContext(turn) => Some(turn),
            _ => None,
        });
        let model = latest_turn.map(|turn| turn.model.clone());
        let reasoning_effort = latest_turn.and_then(|turn| turn.effort.clone());
        let history = include_history.then(|| StoredThreadHistory {
            thread_id,
            items: entry.items.clone(),
        });
        let (forked_from_id, parent_thread_id, source, thread_source, cwd, model_provider) =
            match (&entry.created, meta) {
                (Some(created), _) => (
                    created.forked_from_id,
                    created.parent_thread_id,
                    created.source.clone(),
                    created.thread_source.clone(),
                    created.cwd.clone(),
                    created.model_provider.clone(),
                ),
                (None, Some(meta)) => (
                    meta.forked_from_id,
                    meta.parent_thread_id,
                    meta.source.clone(),
                    meta.thread_source.clone(),
                    meta.cwd.clone(),
                    meta.model_provider.clone().unwrap_or_default(),
                ),
                (None, None) => return Err(ThreadStoreError::ThreadNotFound { thread_id }),
            };
        Ok(StoredThread {
            thread_id,
            forked_from_id,
            parent_thread_id,
            agent_nickname: source.get_nickname(),
            agent_role: source.get_agent_role(),
            agent_path: source.get_agent_path().map(Into::into),
            source,
            thread_source,
            history_mode: entry.history_mode,
            cwd,
            model_provider,
            model,
            reasoning_effort,
            history,
        })
    }

    /// Forgets a thread whose session never finished starting.
    pub async fn discard_thread(&self, thread_id: ThreadId) {
        self.threads.lock().await.remove(&thread_id);
    }
}

fn history_mode_from_items(items: &[RolloutItem]) -> ThreadHistoryMode {
    // Forked histories keep the source SessionMeta after the new thread's own, so the thread
    // contract comes from the first one.
    items
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(line) => Some(line.meta.history_mode),
            _ => None,
        })
        .unwrap_or_default()
}

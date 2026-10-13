use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::into_persisted_rollout_items;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LoadThreadHistoryParams;
use crate::MoveThreadToSectionParams;
use crate::PersistContext;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;

static IN_MEMORY_THREAD_STORES: OnceLock<Mutex<HashMap<String, Arc<InMemoryThreadStore>>>> =
    OnceLock::new();

fn stores() -> &'static Mutex<HashMap<String, Arc<InMemoryThreadStore>>> {
    IN_MEMORY_THREAD_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stores_guard() -> MutexGuard<'static, HashMap<String, Arc<InMemoryThreadStore>>> {
    match stores().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Recorded call counts for [`InMemoryThreadStore`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryThreadStoreCalls {
    pub create_thread: usize,
    pub resume_thread: usize,
    pub append_items: usize,
    pub persist_thread: usize,
    pub flush_thread: usize,
    pub shutdown_thread: usize,
    pub discard_thread: usize,
    pub load_history: usize,
    pub load_latest_model_context: usize,
    pub read_thread: usize,
    pub read_thread_with_history: usize,
    pub read_thread_by_rollout_path: usize,
    pub list_threads: usize,
    pub update_thread_metadata: usize,
    pub archive_thread: usize,
    pub unarchive_thread: usize,
    pub delete_thread: usize,
}

/// In-memory [`ThreadStore`] implementation for tests and debug configs.
///
/// Test and debug configs can select this store by id, letting tests exercise
/// config-driven non-local persistence without requiring the real remote gRPC
/// service.
#[derive(Default)]
pub struct InMemoryThreadStore {
    state: Arc<tokio::sync::Mutex<InMemoryThreadStoreState>>,
    omit_metadata_update_result: Arc<AtomicBool>,
    state_db: Option<codex_rollout::StateDbHandle>,
}

#[derive(Default)]
struct InMemoryThreadStoreState {
    calls: InMemoryThreadStoreCalls,
    repeated_list_threads_cursor: Option<String>,
    created_threads: HashMap<ThreadId, CreateThreadParams>,
    histories: HashMap<ThreadId, Vec<RolloutItem>>,
    metadata_updates: HashMap<ThreadId, ThreadMetadataPatch>,
    sections: HashMap<ThreadId, String>,
    section_positions: HashMap<ThreadId, i64>,
    section_entered_at: HashMap<ThreadId, DateTime<Utc>>,
    names: HashMap<ThreadId, Option<String>>,
    rollout_paths: HashMap<PathBuf, ThreadId>,
}

impl InMemoryThreadStore {
    /// Returns the store associated with `id`, creating it if needed.
    pub fn for_id(id: impl Into<String>) -> Arc<Self> {
        let id = id.into();
        let mut stores = stores_guard();
        stores
            .entry(id)
            .or_insert_with(|| Arc::new(Self::default()))
            .clone()
    }

    /// Shares this debug store's thread data while owning cleanup of the caller's SQLite state.
    pub fn with_state_db(&self, state_db: Option<codex_rollout::StateDbHandle>) -> Self {
        Self {
            state: Arc::clone(&self.state),
            omit_metadata_update_result: Arc::clone(&self.omit_metadata_update_result),
            state_db,
        }
    }

    /// Removes a shared in-memory store for `id`.
    pub fn remove_id(id: &str) -> Option<Arc<Self>> {
        stores_guard().remove(id)
    }

    /// Returns the calls observed by this store.
    pub async fn calls(&self) -> InMemoryThreadStoreCalls {
        self.state.lock().await.calls.clone()
    }

    async fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        let mut state = self.state.lock().await;
        state.calls.create_thread += 1;
        let session_meta = SessionMeta {
            session_id: params.session_id,
            id: params.thread_id,
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            cwd: params.metadata.cwd.clone().unwrap_or_default(),
            runtime_workspace_roots: params
                .runtime_workspace_roots
                .as_ref()
                .map(|roots| roots.iter().map(AbsolutePathBuf::to_path_buf).collect()),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            originator: params.originator.clone(),
            creator_user_id: params.creator_user_id.clone(),
            creator_account_id: params.creator_account_id.clone(),
            source: params.source.clone(),
            thread_source: params.thread_source.clone(),
            model_provider: Some(params.metadata.model_provider.clone()),
            base_instructions: Some(params.base_instructions.clone()),
            dynamic_tools: (!params.dynamic_tools.is_empty()).then(|| params.dynamic_tools.clone()),
            selected_capability_roots: params.selected_capability_roots.clone(),
            memory_mode: matches!(params.metadata.memory_mode, ThreadMemoryMode::Disabled)
                .then_some("disabled".to_string()),
            history_mode: params.history_mode,
            history_base: params.history_base,
            subagent_history_start_ordinal: params.subagent_history_start_ordinal,
            multi_agent_version: params.multi_agent_version,
            context_window: Some(SessionContextWindow::new(params.initial_window_id.clone())),
            ..SessionMeta::default()
        };
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .push(RolloutItem::SessionMeta(SessionMetaLine {
                meta: session_meta,
                git: None,
            }));
        state.created_threads.insert(params.thread_id, params);
        Ok(())
    }

    async fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreResult<Arc<Vec<RolloutItem>>> {
        let mut state = self.state.lock().await;
        state.calls.resume_thread += 1;
        if let Some(history) = params.history {
            if matches!(
                history.first(),
                Some(RolloutItem::SessionMeta(meta)) if meta.meta.id == params.thread_id
            ) {
                state
                    .histories
                    .entry(params.thread_id)
                    .or_insert_with(|| Arc::unwrap_or_clone(history));
            } else {
                state
                    .histories
                    .insert(params.thread_id, Arc::unwrap_or_clone(history));
            }
        } else {
            state.histories.entry(params.thread_id).or_default();
        }
        if let Some(rollout_path) = params.rollout_path {
            state.rollout_paths.insert(rollout_path, params.thread_id);
        }
        Ok(Arc::new(state.histories[&params.thread_id].clone()))
    }

    async fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreResult<()> {
        if params.items.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        let persisted_items = into_persisted_rollout_items(params.items, history_mode);
        if persisted_items.is_empty() {
            return Ok(());
        }
        state.calls.append_items += 1;
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .extend(persisted_items);
        Ok(())
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        let mut state = self.state.lock().await;
        state.calls.load_history += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        reject_paginated_history_mode(history_mode)?;
        Ok(StoredThreadHistory {
            revision: None,
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredModelContext> {
        let mut state = self.state.lock().await;
        state.calls.load_latest_model_context += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        Ok(StoredModelContext {
            revision: None,
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread += 1;
        if params.include_history {
            state.calls.read_thread_with_history += 1;
            reject_paginated_history_mode(history_mode_from_state(&state, params.thread_id))?;
        }
        let thread = stored_thread_from_state(&state, params.thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread_by_rollout_path += 1;
        let Some(thread_id) = state.rollout_paths.get(&params.rollout_path).copied() else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "in-memory thread store does not know rollout path {}",
                    params.rollout_path.display()
                ),
            });
        };
        if params.include_history {
            reject_paginated_history_mode(history_mode_from_state(&state, thread_id))?;
        }
        let thread = stored_thread_from_state(&state, thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn list_threads(&self) -> ThreadStoreResult<ThreadPage> {
        let mut state = self.state.lock().await;
        state.calls.list_threads += 1;
        let mut items = state
            .created_threads
            .keys()
            .map(|thread_id| {
                stored_thread_from_state(&state, *thread_id, /*include_history*/ false)
            })
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        items.sort_by_key(|item| item.thread_id.to_string());
        Ok(ThreadPage {
            items,
            next_cursor: state.repeated_list_threads_cursor.clone(),
        })
    }

    async fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreResult<StoredThread> {
        if params.patch.project_id.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "projects",
            });
        }
        let mut state = self.state.lock().await;
        state.calls.update_thread_metadata += 1;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        if let Some(name) = params.patch.name.clone() {
            state.names.insert(params.thread_id, name);
        }
        state
            .metadata_updates
            .entry(params.thread_id)
            .or_default()
            .merge(params.patch);
        stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
    }

    async fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreResult<()> {
        if params
            .section
            .as_deref()
            .is_some_and(|section| section.trim().is_empty())
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "section must not be empty".to_owned(),
            });
        }
        if params.section.is_none() && params.before_thread_id.is_some() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "before thread cannot be specified without a section".to_owned(),
            });
        }

        let mut state = self.state.lock().await;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        let previous_section = state.sections.get(&params.thread_id).cloned();
        let Some(section) = params.section.as_deref() else {
            state.sections.remove(&params.thread_id);
            state.section_positions.remove(&params.thread_id);
            state.section_entered_at.remove(&params.thread_id);
            return Ok(());
        };
        if let Some(before_thread_id) = params.before_thread_id {
            if before_thread_id == params.thread_id {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {} cannot be moved before itself", params.thread_id),
                });
            }
            let before_section = state.sections.get(&before_thread_id).map(String::as_str);
            if before_section != Some(section) {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!(
                        "before thread {before_thread_id} is not in section {section}"
                    ),
                });
            }
        }

        let mut ordered_thread_ids = state
            .sections
            .iter()
            .filter(|(thread_id, current_section)| {
                **thread_id != params.thread_id && current_section.as_str() == section
            })
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        ordered_thread_ids.sort_by_key(|thread_id| {
            (
                state
                    .section_positions
                    .get(thread_id)
                    .copied()
                    .unwrap_or(i64::MAX),
                thread_id.to_string(),
            )
        });
        let insert_at = params
            .before_thread_id
            .and_then(|before_thread_id| {
                ordered_thread_ids
                    .iter()
                    .position(|thread_id| *thread_id == before_thread_id)
            })
            .unwrap_or(ordered_thread_ids.len());
        ordered_thread_ids.insert(insert_at, params.thread_id);
        for (index, thread_id) in ordered_thread_ids.into_iter().enumerate() {
            let position = i64::try_from(index)
                .unwrap_or(i64::MAX)
                .saturating_add(1)
                .saturating_mul(1_000_000);
            state.section_positions.insert(thread_id, position);
        }
        if previous_section.as_deref() != Some(section) {
            state
                .section_entered_at
                .insert(params.thread_id, Utc::now());
            state.sections.insert(params.thread_id, section.to_owned());
        }
        Ok(())
    }

    async fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreResult<()> {
        self.state.lock().await.calls.delete_thread += 1;
        let deleted_state_rows = if let Some(state_db) = &self.state_db {
            state_db
                .delete_threads_strict(&[params.thread_id])
                .await
                .map_err(|error| ThreadStoreError::Internal {
                    message: format!("failed to delete thread state: {error}"),
                })?
        } else {
            0
        };
        let mut state = self.state.lock().await;
        let existed = state.histories.remove(&params.thread_id).is_some();
        state.created_threads.remove(&params.thread_id);
        state.names.remove(&params.thread_id);
        state.metadata_updates.remove(&params.thread_id);
        state.sections.remove(&params.thread_id);
        state.section_positions.remove(&params.thread_id);
        state.section_entered_at.remove(&params.thread_id);
        state
            .rollout_paths
            .retain(|_, thread_id| *thread_id != params.thread_id);
        if existed || deleted_state_rows > 0 {
            Ok(())
        } else {
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            })
        }
    }
}

impl ThreadStore for InMemoryThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::create_thread(self, params))
    }

    fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>> {
        Box::pin(InMemoryThreadStore::resume_thread(self, params))
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::append_items(self, params))
    }

    fn persist_thread(
        &self,
        _thread_id: ThreadId,
        _context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.persist_thread += 1;
            Ok(())
        })
    }

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.flush_thread += 1;
            Ok(())
        })
    }

    fn shutdown_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.shutdown_thread += 1;
            Ok(())
        })
    }

    fn discard_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.discard_thread += 1;
            Ok(())
        })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(InMemoryThreadStore::load_history(self, params))
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(InMemoryThreadStore::load_latest_model_context(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread(self, params))
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread_by_rollout_path(
            self, params,
        ))
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async move {
            let mut page = InMemoryThreadStore::list_threads(self).await?;
            match params.relation_filter {
                Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
                    page.items
                        .retain(|thread| thread.parent_thread_id == Some(parent_thread_id));
                }
                Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
                    let mut subtree = HashSet::from([ancestor_thread_id]);
                    loop {
                        let mut discovered = false;
                        for thread in &page.items {
                            if thread
                                .parent_thread_id
                                .is_some_and(|parent_thread_id| subtree.contains(&parent_thread_id))
                            {
                                discovered |= subtree.insert(thread.thread_id);
                            }
                        }
                        if !discovered {
                            break;
                        }
                    }
                    page.items.retain(|thread| {
                        thread.thread_id != ancestor_thread_id
                            && subtree.contains(&thread.thread_id)
                    });
                }
                None => {}
            }
            if let Some(section) = params.section.as_ref() {
                page.items.retain(|thread| {
                    thread.section.as_ref().map(|section| section.id.as_str()) == section.as_deref()
                });
            }
            if params.sort_key == ThreadSortKey::SectionPosition {
                page.items.sort_by_key(|thread| {
                    (
                        thread.section_position.unwrap_or(i64::MAX),
                        thread.thread_id.to_string(),
                    )
                });
                if params.sort_direction == crate::SortDirection::Desc {
                    page.items.reverse();
                }
            }
            Ok(page)
        })
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            let updated = InMemoryThreadStore::update_thread_metadata(self, params).await?;
            Ok((!self.omit_metadata_update_result.load(Ordering::Relaxed)).then_some(updated))
        })
    }

    fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::move_thread_to_section(self, params))
    }

    fn archive_thread(&self, _params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.archive_thread += 1;
            Ok(())
        })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.unarchive_thread += 1;
            stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
        })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::delete_thread(self, params))
    }
}

fn stored_thread_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
    include_history: bool,
) -> ThreadStoreResult<StoredThread> {
    let created = state
        .created_threads
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let history_items = state.histories.get(&thread_id).cloned().unwrap_or_default();
    let history = include_history.then(|| StoredThreadHistory {
        revision: None,
        thread_id,
        items: history_items.clone(),
    });
    let name = state.names.get(&thread_id).cloned().flatten();
    let metadata = state.metadata_updates.get(&thread_id);
    let rollout_path = state
        .rollout_paths
        .iter()
        .find_map(|(path, mapped_thread_id)| {
            (*mapped_thread_id == thread_id).then(|| path.clone())
        });

    Ok(StoredThread {
        originator: (!created.originator.is_empty()).then(|| created.originator.clone()),
        thread_id,
        extra_config: created.extra_config.clone(),
        rollout_path: metadata
            .and_then(|metadata| metadata.rollout_path.clone())
            .or(rollout_path),
        forked_from_id: created.forked_from_id,
        parent_thread_id: created.parent_thread_id,
        preview: metadata
            .and_then(|metadata| metadata.preview.clone())
            .unwrap_or_default(),
        name,
        model_provider: metadata
            .and_then(|metadata| metadata.model_provider.clone())
            .unwrap_or_else(|| "test".to_string()),
        model: metadata.and_then(|metadata| metadata.model.clone()),
        reasoning_effort: metadata
            .and_then(|metadata| metadata.reasoning_effort.clone())
            .flatten(),
        created_at: metadata
            .and_then(|metadata| metadata.created_at)
            .unwrap_or_else(Utc::now),
        updated_at: metadata
            .and_then(|metadata| metadata.updated_at)
            .unwrap_or_else(Utc::now),
        recency_at: metadata
            .and_then(|metadata| metadata.advance_recency_at.or(metadata.updated_at))
            .unwrap_or_else(Utc::now),
        archived_at: None,
        section: state
            .sections
            .get(&thread_id)
            .cloned()
            .map(|id| codex_state::ThreadSection {
                name: if id == codex_state::PINNED_THREAD_SECTION_ID {
                    codex_state::PINNED_THREAD_SECTION_NAME.to_string()
                } else {
                    id.clone()
                },
                id,
                appearance: None,
            }),
        section_position: state.section_positions.get(&thread_id).copied(),
        section_entered_at: state.section_entered_at.get(&thread_id).copied(),
        project_id: None,
        daybreak_enabled: metadata.and_then(|metadata| metadata.daybreak_enabled),
        cwd: metadata
            .and_then(|metadata| metadata.cwd.clone())
            .unwrap_or_default(),
        cli_version: metadata
            .and_then(|metadata| metadata.cli_version.clone())
            .unwrap_or_else(|| "test".to_string()),
        source: metadata
            .and_then(|metadata| metadata.source.clone())
            .unwrap_or_else(|| created.source.clone()),
        history_mode: created.history_mode,
        thread_source: metadata
            .and_then(|metadata| metadata.thread_source.clone())
            .unwrap_or_else(|| created.thread_source.clone()),
        agent_nickname: metadata.and_then(|metadata| metadata.agent_nickname.clone().flatten()),
        agent_role: metadata.and_then(|metadata| metadata.agent_role.clone().flatten()),
        agent_path: metadata.and_then(|metadata| metadata.agent_path.clone().flatten()),
        git_info: metadata.and_then(git_info_from_patch),
        token_usage: metadata.and_then(|metadata| metadata.token_usage.clone()),
        first_user_message: metadata.and_then(|metadata| metadata.first_user_message.clone()),
        history,
    })
}

fn history_mode_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
) -> ThreadHistoryMode {
    state
        .created_threads
        .get(&thread_id)
        .map(|thread| thread.history_mode)
        .unwrap_or_default()
}

fn git_info_from_patch(patch: &ThreadMetadataPatch) -> Option<codex_protocol::protocol::GitInfo> {
    let git_info = patch.git_info.as_ref()?;
    let sha = git_info.sha.clone().flatten();
    let branch = git_info.branch.clone().flatten();
    let origin_url = git_info.origin_url.clone().flatten();
    if sha.is_none() && branch.is_none() && origin_url.is_none() {
        return None;
    }
    Some(codex_protocol::protocol::GitInfo {
        commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
        branch,
        repository_url: origin_url,
    })
}

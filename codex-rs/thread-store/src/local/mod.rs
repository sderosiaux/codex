mod archive_thread;
mod create_thread;
mod delete_thread;
mod helpers;
mod history_revision;
mod list_threads;
mod live_writer;
mod model_context;
mod move_thread_to_section;
mod paginated_fork;
mod pending_thread_metadata;
mod projects;
mod read_thread;
mod revert_thread;
mod rollout_migration;
// This lands before the reader PRs that consume the shared lineage resolver.
#[allow(dead_code)]
mod rollout_lineage;
mod search_threads;
mod thread_attachments;
mod thread_history;
mod thread_history_materialization;
mod thread_rollout_resolver;
mod thread_sections;
mod unarchive_thread;
mod update_thread_metadata;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_rollout::StateDbHandle;
use codex_rollout::WriterLockCoordinator;
use codex_state::SqliteConfig;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::OwnedRwLockReadGuard;
use tokio::sync::OwnedRwLockWriteGuard;
use tokio::sync::RwLock;

use crate::AddThreadAttachmentOutcome;
use crate::AddThreadAttachmentParams;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ArchiveThreadsParams;
use crate::CreateProjectParams;
use crate::CreateThreadParams;
use crate::CreateThreadSectionParams;
use crate::CreatedProject;
use crate::DeleteThreadParams;
use crate::DeleteThreadSectionParams;
use crate::DeleteThreadsParams;
use crate::DeletedProject;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListProjectsParams;
use crate::ListThreadAttachmentThreadsParams;
use crate::ListThreadAttachmentsParams;
use crate::ListThreadSectionsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::MoveProjectParams;
use crate::MoveThreadToSectionParams;
use crate::PersistContext;
use crate::PrepareForkParams;
use crate::PreparedFork;
use crate::ProjectMoveOutcome;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::RemoveThreadAttachmentOutcome;
use crate::RemoveThreadAttachmentParams;
use crate::RenameThreadSectionParams;
use crate::ResumeThreadParams;
use crate::RevertThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadAttachmentOwnerPage;
use crate::ThreadAttachmentPage;
use crate::ThreadMetadataPatch;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::TurnPage;
use crate::UpdateProjectParams;
use crate::UpdateThreadMetadataParams;
use crate::UpdatedProject;

pub use rollout_migration::RolloutMigrationFailureReason;
pub use rollout_migration::RolloutMigrationMode;
pub use rollout_migration::RolloutMigrationOptions;
pub use rollout_migration::RolloutMigrationOutcome;
pub use rollout_migration::RolloutMigrationProgress;
pub use rollout_migration::RolloutMigrationReport;
pub use rollout_migration::RolloutMigrationStatus;

/// Local filesystem/SQLite-backed implementation of [`ThreadStore`].
///
/// Local storage has two compatibility surfaces. Rollout JSONL files are the
/// durable replay format and remain readable without SQLite, including older
/// files that encode metadata in `SessionMeta` items and name-index entries.
/// The SQLite state DB, when available, is the queryable metadata index used by
/// list/read paths for fast lookup.
///
/// Live appends still write canonical JSONL history, but append-derived
/// metadata is observed above the store and applied through
/// [`ThreadStore::update_thread_metadata`]. This implementation applies that
/// patch literally to SQLite while keeping the JSONL/name-index compatibility
/// behavior needed for SQLite-less reads, repair, and old local rollout files.
#[derive(Clone)]
pub struct LocalThreadStore {
    pub(super) config: LocalThreadStoreConfig,
    live_recorders: Arc<Mutex<HashMap<ThreadId, LiveRecorderEntry>>>,
    pending_thread_metadata: pending_thread_metadata::PendingThreadMetadataRegistry,
    live_writer_locks: Arc<LiveWriterLocks>,
    writer_lock_coordinator: Arc<WriterLockCoordinator>,
    state_db: Option<StateDbHandle>,
    thread_history_db: Arc<OnceCell<sqlx::SqlitePool>>,
    thread_data_cleanup: Option<Arc<ThreadDataCleanup>>,
}

type ThreadDataCleanup = dyn Fn(Vec<ThreadId>) -> ThreadStoreFuture<'static, ()> + Send + Sync;

type WriterLockGuard = Arc<codex_rollout::WriterLockGuard>;

struct LiveRecorderEntry {
    recorder: RolloutRecorder,
    // Rollout projection rows are keyed by immutable rollout ID, not the stable thread ID used
    // to find this live writer.
    rollout_id: ThreadId,
    // Local rollout files are materialized lazily, but metadata updates can arrive before the
    // canonical SessionMeta is durable. Retain the mode captured when live persistence was opened
    // so missing SQLite rows can still be seeded.
    history_mode: ThreadHistoryMode,
    writer_lock: WriterLockGuard,
}

#[derive(Default)]
struct LiveWriterLocks {
    // Keep per-thread locks after a writer goes idle. Removing one while another caller is about
    // to acquire it could let two operations for the same thread run at once.
    by_thread: Mutex<HashMap<ThreadId, Arc<ThreadCoordination>>>,
}

#[derive(Default)]
struct ThreadCoordination {
    // Serialize writes and capture consistent fork snapshots.
    writer: Arc<Mutex<()>>,
    // Forks hold a shared lease until their child reference is durable; deletion, archive, and
    // unarchive require exclusive access. Keeping this separate from `writer` lets the source
    // accept writes during child initialization, including MCP startup that can take 30 seconds.
    // Operations that need both locks must acquire `lifecycle` before `writer`.
    lifecycle: Arc<RwLock<()>>,
}

impl LiveWriterLocks {
    async fn coordination(&self, thread_id: ThreadId) -> Arc<ThreadCoordination> {
        self.by_thread
            .lock()
            .await
            .entry(thread_id)
            .or_default()
            .clone()
    }

    async fn lock(&self, thread_id: ThreadId) -> OwnedMutexGuard<()> {
        self.coordination(thread_id)
            .await
            .writer
            .clone()
            .lock_owned()
            .await
    }

    async fn reserve_lifecycle(&self, thread_id: ThreadId) -> OwnedRwLockReadGuard<()> {
        self.coordination(thread_id)
            .await
            .lifecycle
            .clone()
            .read_owned()
            .await
    }

    async fn lock_lifecycle(&self, thread_id: ThreadId) -> OwnedRwLockWriteGuard<()> {
        self.coordination(thread_id)
            .await
            .lifecycle
            .clone()
            .write_owned()
            .await
    }
}

/// Process-scoped configuration for local thread storage.
///
/// This describes where local storage lives. New-thread rollout metadata such
/// as cwd, provider, and memory mode is supplied when live persistence is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalThreadStoreConfig {
    pub codex_home: PathBuf,
    pub sqlite: SqliteConfig,
    /// Provider used only when older local metadata does not contain one.
    pub default_model_provider_id: String,
}

impl LocalThreadStoreConfig {
    pub fn from_config(config: &impl codex_rollout::RolloutConfigView) -> Self {
        Self {
            codex_home: config.codex_home().to_path_buf(),
            sqlite: config.sqlite_config().clone(),
            default_model_provider_id: config.model_provider_id().to_string(),
        }
    }
}

impl std::fmt::Debug for LocalThreadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalThreadStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LocalThreadStore {
    /// Create a local store using an already initialized state DB handle.
    pub fn new(config: LocalThreadStoreConfig, state_db: Option<StateDbHandle>) -> Self {
        let writer_lock_coordinator = Arc::new(WriterLockCoordinator::new(&config.codex_home));
        Self {
            config,
            live_recorders: Arc::new(Mutex::new(HashMap::new())),
            pending_thread_metadata:
                pending_thread_metadata::PendingThreadMetadataRegistry::default(),
            live_writer_locks: Arc::new(LiveWriterLocks::default()),
            writer_lock_coordinator,
            state_db,
            thread_history_db: Arc::new(OnceCell::new()),
            thread_data_cleanup: None,
        }
    }

    /// Adds cleanup for host-owned durable data when threads are permanently deleted.
    /// Runs after reference and writer checks, under the lifecycle locks, before deleting rollouts.
    /// Cleanup must be idempotent: later deletion steps can fail and the caller may retry.
    pub fn with_thread_data_cleanup(
        mut self,
        cleanup: impl Fn(Vec<ThreadId>) -> ThreadStoreFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        self.thread_data_cleanup = Some(Arc::new(cleanup));
        self
    }

    /// Return the state DB handle used by local rollout writers.
    pub async fn state_db(&self) -> Option<StateDbHandle> {
        self.state_db.clone()
    }

    async fn thread_history_db(&self) -> ThreadStoreResult<&sqlx::SqlitePool> {
        if self.state_db.is_none() {
            return Err(ThreadStoreError::Unsupported {
                operation: "paginated_history",
            });
        }
        self.thread_history_db
            .get_or_try_init(|| async {
                codex_state::open_thread_history_db(&self.config.sqlite).await
            })
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to open thread history database: {err}"),
            })
    }

    /// Read a local rollout-backed thread by path.
    pub async fn read_thread_by_rollout_path(
        &self,
        rollout_path: PathBuf,
        include_archived: bool,
        include_history: bool,
    ) -> ThreadStoreResult<StoredThread> {
        read_thread::read_thread_by_rollout_path(
            self,
            rollout_path,
            include_archived,
            include_history,
        )
        .await
    }

    /// Return the live local rollout path for legacy local-only code paths.
    pub async fn live_rollout_path(&self, thread_id: ThreadId) -> ThreadStoreResult<PathBuf> {
        live_writer::rollout_path(self, thread_id).await
    }

    pub(super) async fn ensure_live_recorder_absent(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreResult<()> {
        if self.live_recorders.lock().await.contains_key(&thread_id) {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {thread_id} already has a live local writer"),
            });
        }
        Ok(())
    }

    fn acquire_writer_lock(&self, thread_id: ThreadId) -> ThreadStoreResult<WriterLockGuard> {
        self.writer_lock_coordinator
            .acquire(thread_id)
            .map(Arc::new)
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    ThreadStoreError::Conflict {
                        message: err.to_string(),
                    }
                } else {
                    ThreadStoreError::Internal {
                        message: err.to_string(),
                    }
                }
            })
    }

    async fn acquire_writer_locks(
        &self,
        thread_ids: &[ThreadId],
    ) -> ThreadStoreResult<Vec<WriterLockGuard>> {
        let mut writer_locks = Vec::with_capacity(thread_ids.len());
        for &thread_id in thread_ids {
            if self.live_recorders.lock().await.contains_key(&thread_id) {
                continue;
            }
            writer_locks.push(self.acquire_writer_lock(thread_id)?);
        }
        Ok(writer_locks)
    }

    async fn insert_live_recorder(
        &self,
        thread_id: ThreadId,
        recorder: RolloutRecorder,
        rollout_id: ThreadId,
        history_mode: ThreadHistoryMode,
        writer_lock: WriterLockGuard,
    ) -> ThreadStoreResult<()> {
        match self.live_recorders.lock().await.entry(thread_id) {
            Entry::Occupied(entry) => Err(ThreadStoreError::InvalidRequest {
                message: format!("thread {} already has a live local writer", entry.key()),
            }),
            Entry::Vacant(entry) => {
                entry.insert(LiveRecorderEntry {
                    recorder,
                    rollout_id,
                    history_mode,
                    writer_lock,
                });
                Ok(())
            }
        }
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        if let Ok(rollout_path) = live_writer::rollout_path(self, params.thread_id).await {
            if !params.include_archived
                && helpers::rollout_path_is_archived(
                    self.config.codex_home.as_path(),
                    rollout_path.as_path(),
                )
            {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {} is archived", params.thread_id),
                });
            }
            return read_thread::read_thread_by_rollout_path(
                self,
                rollout_path,
                /*include_archived*/ true,
                /*include_history*/ true,
            )
            .await?
            .history
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("failed to load history for thread {}", params.thread_id),
            });
        }

        read_thread::read_thread(
            self,
            ReadThreadParams {
                thread_id: params.thread_id,
                include_archived: params.include_archived,
                include_history: true,
            },
        )
        .await?
        .history
        .ok_or_else(|| ThreadStoreError::Internal {
            message: format!("failed to load history for thread {}", params.thread_id),
        })
    }

    async fn read_thread_by_rollout_path_params(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        read_thread::read_thread_by_rollout_path(
            self,
            params.rollout_path,
            params.include_archived,
            params.include_history,
        )
        .await
    }

    /// Lists projection-backed turns without enabling app-server routing yet.
    pub async fn list_turns(&self, params: ListTurnsParams) -> ThreadStoreResult<TurnPage> {
        thread_history::list_turns(self, params).await
    }

    /// Lists projection-backed items without enabling app-server routing yet.
    pub async fn list_items(&self, params: ListItemsParams) -> ThreadStoreResult<ItemPage> {
        thread_history::list_items(self, params).await
    }

    /// Searches projection-backed visible messages within one paginated thread.
    pub async fn search_thread_occurrences(
        &self,
        params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreResult<ThreadOccurrenceSearchPage> {
        thread_history::search_thread_occurrences(self, params).await
    }
}

impl ThreadStore for LocalThreadStore {
    fn default_history_mode(&self) -> ThreadHistoryMode {
        ThreadHistoryMode::Paginated
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { live_writer::create_thread(self, params).await })
    }

    fn stage_pending_thread_metadata(
        &self,
        thread_id: ThreadId,
        patch: ThreadMetadataPatch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            if self.state_db.is_none() {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "pending thread metadata requires a state db".to_string(),
                });
            }
            if patch.rollout_path.is_some() {
                return Err(ThreadStoreError::InvalidRequest {
                    message: "pending thread metadata cannot set rollout_path".to_string(),
                });
            }
            self.pending_thread_metadata.stage(thread_id, patch).await
        })
    }

    fn read_pending_thread_metadata(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<ThreadMetadataPatch>> {
        Box::pin(async move {
            Ok(self
                .pending_thread_metadata
                .lock(thread_id)
                .await
                .and_then(|metadata| metadata.clone()))
        })
    }

    fn remove_pending_thread_metadata(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.pending_thread_metadata.remove(thread_id).await;
            Ok(())
        })
    }

    fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> ThreadStoreFuture<'_, Arc<Vec<RolloutItem>>> {
        Box::pin(async move { live_writer::resume_thread(self, params).await })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { live_writer::append_items(self, params).await })
    }

    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        if context == PersistContext::SubagentSpawn {
            return Box::pin(async { Ok(()) });
        }
        Box::pin(async move {
            if context == PersistContext::ThreadPreparation {
                live_writer::flush_thread(self, thread_id).await
            } else {
                live_writer::persist_thread(self, thread_id).await
            }
        })
    }

    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { live_writer::flush_thread(self, thread_id).await })
    }

    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { live_writer::shutdown_thread(self, thread_id).await })
    }

    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { live_writer::discard_thread(self, thread_id).await })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(LocalThreadStore::load_history(self, params))
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(async move { model_context::load_latest_model_context(self, params).await })
    }

    fn prepare_fork(&self, params: PrepareForkParams) -> ThreadStoreFuture<'_, PreparedFork> {
        Box::pin(async move { paginated_fork::prepare(self, params).await })
    }

    fn revert_thread(&self, params: RevertThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { revert_thread::revert(self, params).await })
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { read_thread::read_thread(self, params).await })
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(LocalThreadStore::read_thread_by_rollout_path_params(
            self, params,
        ))
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async move { list_threads::list_threads(self, params).await })
    }

    fn supports_thread_sections(&self) -> bool {
        self.state_db.is_some()
    }

    fn list_thread_sections(
        &self,
        params: ListThreadSectionsParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSectionsPage> {
        Box::pin(async move { thread_sections::list_thread_sections(self, params).await })
    }

    fn create_thread_section(
        &self,
        params: CreateThreadSectionParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSection> {
        Box::pin(async move { thread_sections::create_thread_section(self, params).await })
    }

    fn rename_thread_section(
        &self,
        params: RenameThreadSectionParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThreadSection>> {
        Box::pin(async move { thread_sections::rename_thread_section(self, params).await })
    }

    fn delete_thread_section(
        &self,
        params: DeleteThreadSectionParams,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async move { thread_sections::delete_thread_section(self, params).await })
    }

    fn supports_thread_attachments(&self) -> bool {
        self.state_db.is_some()
    }

    fn copy_thread_attachments(
        &self,
        source_thread_id: ThreadId,
        destination_thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            thread_attachments::copy_thread_attachments(
                self,
                source_thread_id,
                destination_thread_id,
            )
            .await
        })
    }

    fn add_thread_attachment(
        &self,
        params: AddThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, AddThreadAttachmentOutcome> {
        Box::pin(async move { thread_attachments::add_thread_attachment(self, params).await })
    }

    fn list_thread_attachments(
        &self,
        params: ListThreadAttachmentsParams,
    ) -> ThreadStoreFuture<'_, ThreadAttachmentPage> {
        Box::pin(async move { thread_attachments::list_thread_attachments(self, params).await })
    }

    fn list_thread_attachment_threads(
        &self,
        params: ListThreadAttachmentThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadAttachmentOwnerPage> {
        Box::pin(
            async move { thread_attachments::list_thread_attachment_threads(self, params).await },
        )
    }

    fn remove_thread_attachment(
        &self,
        params: RemoveThreadAttachmentParams,
    ) -> ThreadStoreFuture<'_, RemoveThreadAttachmentOutcome> {
        Box::pin(async move { thread_attachments::remove_thread_attachment(self, params).await })
    }

    fn supports_projects(&self) -> bool {
        self.state_db.is_some()
    }

    fn list_projects(
        &self,
        params: ListProjectsParams,
    ) -> ThreadStoreFuture<'_, StoredProjectsPage> {
        Box::pin(async move { projects::list_projects(self, params).await })
    }

    fn read_project(&self, project_id: String) -> ThreadStoreFuture<'_, Option<StoredProject>> {
        Box::pin(async move { projects::read_project(self, project_id).await })
    }

    fn create_project(&self, params: CreateProjectParams) -> ThreadStoreFuture<'_, CreatedProject> {
        Box::pin(async move { projects::create_project(self, params).await })
    }

    fn update_project(
        &self,
        params: UpdateProjectParams,
    ) -> ThreadStoreFuture<'_, Option<UpdatedProject>> {
        Box::pin(async move { projects::update_project(self, params).await })
    }

    fn move_project(
        &self,
        params: MoveProjectParams,
    ) -> ThreadStoreFuture<'_, Option<ProjectMoveOutcome>> {
        Box::pin(async move { projects::move_project(self, params).await })
    }

    fn delete_project(&self, project_id: String) -> ThreadStoreFuture<'_, Option<DeletedProject>> {
        Box::pin(async move { projects::delete_project(self, project_id).await })
    }

    fn supports_paginated_history_lists(&self) -> bool {
        self.state_db.is_some()
    }

    fn list_turns(&self, params: ListTurnsParams) -> ThreadStoreFuture<'_, TurnPage> {
        Box::pin(LocalThreadStore::list_turns(self, params))
    }

    fn list_items(&self, params: ListItemsParams) -> ThreadStoreFuture<'_, ItemPage> {
        Box::pin(LocalThreadStore::list_items(self, params))
    }

    fn search_threads(
        &self,
        params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(async move { search_threads::search_threads(self, params).await })
    }

    fn search_thread_occurrences(
        &self,
        params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreFuture<'_, ThreadOccurrenceSearchPage> {
        Box::pin(LocalThreadStore::search_thread_occurrences(self, params))
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            update_thread_metadata::update_thread_metadata(self, params)
                .await
                .map(Some)
        })
    }

    fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { move_thread_to_section::move_thread_to_section(self, params).await })
    }

    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            archive_thread::archive_threads(
                self,
                ArchiveThreadsParams {
                    thread_ids: vec![params.thread_id],
                    writer_lock_thread_ids: Vec::new(),
                },
            )
            .await
            .map(|_| ())
        })
    }

    fn archive_threads(
        &self,
        params: ArchiveThreadsParams,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move { archive_thread::archive_threads(self, params).await })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { unarchive_thread::unarchive_thread(self, params).await })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { delete_thread::delete_thread(self, params).await })
    }

    fn delete_threads(&self, params: DeleteThreadsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move { delete_thread::delete_threads(self, params).await })
    }
}

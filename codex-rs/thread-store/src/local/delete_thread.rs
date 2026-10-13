//! Local hard-delete support for persisted threads.
//!
//! Existing rollout files are deleted before this operation reports success. A rollout file that
//! vanishes after discovery counts as already deleted. Main state DB rows are deleted after every
//! associated rollout is removed, under the same lifecycle lock, so queued artifact mutations cannot
//! use deleted thread metadata. Host-owned data is cleaned after reference and writer checks,
//! before removing rollouts, so cleanup failures remain retryable even without a state DB.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use codex_rollout::ARCHIVED_SESSIONS_SUBDIR;
use codex_rollout::RolloutReferenceIndex;
use codex_rollout::SESSIONS_SUBDIR;
use codex_rollout::find_archived_thread_path_by_id_str;
use codex_rollout::find_thread_path_by_id_str;
use codex_rollout::remove_thread_name_entries;

use super::LocalThreadStore;
use super::helpers::scoped_rollout_path;
use super::helpers::validated_rollout_file_name;
use crate::DeleteThreadParams;
use crate::DeleteThreadsParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

struct ThreadRollouts {
    thread_id: codex_protocol::ThreadId,
    rollout_ids: HashSet<codex_protocol::ThreadId>,
    paths: Vec<PathBuf>,
}

impl ThreadRollouts {
    fn from_index(
        reference_index: &RolloutReferenceIndex,
        thread_id: codex_protocol::ThreadId,
    ) -> Self {
        let mut rollout_ids = HashSet::new();
        let paths = reference_index
            .rollouts_for_thread(thread_id)
            .map(|(rollout_id, path)| {
                rollout_ids.insert(rollout_id);
                path.to_path_buf()
            })
            .collect();
        Self {
            thread_id,
            rollout_ids,
            paths,
        }
    }

    fn add_path(&mut self, path: PathBuf) {
        if let Some(rollout_id) = codex_rollout::rollout_id_from_path(path.as_path()) {
            self.rollout_ids.insert(rollout_id);
        }
        if !self.paths.contains(&path) {
            self.paths.push(path);
        }
    }
}

pub(super) async fn delete_thread(
    store: &LocalThreadStore,
    params: DeleteThreadParams,
) -> ThreadStoreResult<()> {
    let thread_id = params.thread_id;
    let _lifecycle_guard = store.live_writer_locks.lock_lifecycle(thread_id).await;
    let _live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    let reference_index = scan_reference_index(store).await?;
    let thread_rollouts = ThreadRollouts::from_index(&reference_index, thread_id);
    ensure_no_external_references(&reference_index, std::slice::from_ref(&thread_rollouts))?;
    let mut writer_guards = store.acquire_writer_locks(&[thread_id]).await?;
    if let Some(cleanup) = &store.thread_data_cleanup {
        cleanup(vec![thread_id]).await?;
    }
    let found_rollout =
        match delete_thread_after_reference_check(store, thread_rollouts, &mut writer_guards).await
        {
            Ok(()) => true,
            Err(ThreadStoreError::ThreadNotFound { .. }) => false,
            Err(err) => return Err(err),
        };
    let deleted_state_rows = delete_state_rows(store, &[thread_id]).await?;
    if found_rollout || deleted_state_rows > 0 {
        Ok(())
    } else {
        Err(ThreadStoreError::ThreadNotFound { thread_id })
    }
}

pub(super) async fn delete_threads(
    store: &LocalThreadStore,
    params: DeleteThreadsParams,
) -> ThreadStoreResult<()> {
    let thread_ids = params.thread_ids;
    if thread_ids.is_empty() {
        return Ok(());
    }

    let mut lock_thread_ids = thread_ids.clone();
    lock_thread_ids.sort_unstable_by_key(ToString::to_string);
    lock_thread_ids.dedup();
    let mut _lifecycle_guards = Vec::with_capacity(lock_thread_ids.len());
    for thread_id in &lock_thread_ids {
        _lifecycle_guards.push(store.live_writer_locks.lock_lifecycle(*thread_id).await);
    }
    let mut _live_writer_guards = Vec::with_capacity(lock_thread_ids.len());
    for &thread_id in &lock_thread_ids {
        _live_writer_guards.push(store.live_writer_locks.lock(thread_id).await);
    }

    let reference_index = scan_reference_index(store).await?;
    let thread_rollouts = thread_ids
        .iter()
        .map(|thread_id| ThreadRollouts::from_index(&reference_index, *thread_id))
        .collect::<Vec<_>>();
    ensure_no_external_references(&reference_index, thread_rollouts.as_slice())?;

    let mut writer_guards = store.acquire_writer_locks(&lock_thread_ids).await?;
    if let Some(cleanup) = &store.thread_data_cleanup {
        cleanup(thread_ids.clone()).await?;
    }
    for thread_rollouts in thread_rollouts {
        match delete_thread_after_reference_check(store, thread_rollouts, &mut writer_guards).await
        {
            Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    // Retain the complete retry graph until every rollout has been removed.
    delete_state_rows(store, &thread_ids).await?;
    Ok(())
}

async fn delete_state_rows(
    store: &LocalThreadStore,
    thread_ids: &[codex_protocol::ThreadId],
) -> ThreadStoreResult<u64> {
    let Some(state_db) = store.state_db.as_ref() else {
        return Ok(0);
    };
    state_db
        .delete_threads_strict(thread_ids)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to delete thread state: {err}"),
        })
}

fn ensure_no_external_references(
    reference_index: &RolloutReferenceIndex,
    thread_rollouts: &[ThreadRollouts],
) -> ThreadStoreResult<()> {
    let deletion_rollout_ids = thread_rollouts
        .iter()
        .flat_map(|thread_rollouts| thread_rollouts.rollout_ids.iter().copied())
        .collect::<HashSet<_>>();
    let mut internal_reference_counts = HashMap::new();
    for source_rollout_id in &deletion_rollout_ids {
        if let Some(history_base) = reference_index.history_base(*source_rollout_id)
            && history_base.thread_id != *source_rollout_id
            && deletion_rollout_ids.contains(&history_base.thread_id)
        {
            *internal_reference_counts
                .entry(history_base.thread_id)
                .or_default() += 1;
        }
    }
    for thread_rollouts in thread_rollouts {
        if thread_rollouts.rollout_ids.iter().any(|rollout_id| {
            let internal_reference_count = internal_reference_counts
                .get(rollout_id)
                .copied()
                .unwrap_or_default();
            reference_index.reference_count(*rollout_id) > internal_reference_count
        }) {
            return Err(referenced_thread_error(thread_rollouts.thread_id));
        }
    }
    Ok(())
}

async fn scan_reference_index(
    store: &LocalThreadStore,
) -> ThreadStoreResult<RolloutReferenceIndex> {
    RolloutReferenceIndex::scan(store.config.codex_home.as_path())
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to scan fork history references: {err}"),
        })
}

fn referenced_thread_error(thread_id: codex_protocol::ThreadId) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("cannot delete thread {thread_id}: forked history still references it"),
    }
}

async fn delete_thread_after_reference_check(
    store: &LocalThreadStore,
    mut thread_rollouts: ThreadRollouts,
    writer_guards: &mut Vec<super::WriterLockGuard>,
) -> ThreadStoreResult<()> {
    let thread_id = thread_rollouts.thread_id;
    let thread_id_str = thread_id.to_string();
    let state_db_ctx = store.state_db().await;
    match find_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) => thread_rollouts.add_path(path),
        Ok(None) => {}
        Err(err) => {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("failed to locate thread id {thread_id}: {err}"),
            });
        }
    }
    match find_archived_thread_path_by_id_str(
        store.config.codex_home.as_path(),
        thread_id_str.as_str(),
        state_db_ctx.as_deref(),
    )
    .await
    {
        Ok(Some(path)) => thread_rollouts.add_path(path),
        Ok(None) => {}
        Err(err) => {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!("failed to locate archived thread id {thread_id}: {err}"),
            });
        }
    }
    thread_rollouts.rollout_ids.insert(thread_id);
    for rollout_id in thread_rollouts.rollout_ids {
        super::thread_history::delete_thread(store, rollout_id).await?;
    }

    // Stop queued file work before removing files, retaining ownership until cleanup finishes.
    let live_entry = store.live_recorders.lock().await.remove(&thread_id);
    if let Some(entry) = live_entry {
        writer_guards.push(entry.writer_lock);
        entry
            .recorder
            .discard()
            .await
            .map_err(|err| ThreadStoreError::Internal {
                message: format!("failed to stop thread writer before deletion: {err}"),
            })?;
    }
    let found_rollout_path = !thread_rollouts.paths.is_empty();
    for rollout_path in thread_rollouts.paths {
        delete_rollout_file(store, rollout_path.as_path())?;
    }
    remove_thread_name_entries(store.config.codex_home.as_path(), thread_id)
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to delete thread name index entries for {thread_id}: {err}"),
        })?;

    if !found_rollout_path {
        return Err(ThreadStoreError::ThreadNotFound { thread_id });
    }

    Ok(())
}

fn delete_rollout_file(store: &LocalThreadStore, rollout_path: &Path) -> ThreadStoreResult<bool> {
    let plain_path = codex_rollout::plain_rollout_path(rollout_path);
    let compressed_path = plain_path.with_extension("jsonl.zst");
    let deleted_plain = delete_rollout_path(store, plain_path.as_path())?;
    let deleted_compressed = delete_rollout_path(store, compressed_path.as_path())?;
    Ok(deleted_plain || deleted_compressed)
}

fn delete_rollout_path(store: &LocalThreadStore, rollout_path: &Path) -> ThreadStoreResult<bool> {
    let canonical_rollout_path = scoped_rollout_path(
        store.config.codex_home.join(SESSIONS_SUBDIR),
        rollout_path,
        "sessions",
    )
    .or_else(|_| {
        scoped_rollout_path(
            store.config.codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
            rollout_path,
            "archived sessions",
        )
    })
    .or_else(|err| match rollout_path.try_exists() {
        Ok(false) => Ok(rollout_path.to_path_buf()),
        Ok(true) | Err(_) => Err(err),
    })?;
    validated_rollout_file_name(&canonical_rollout_path, rollout_path)?;
    match std::fs::remove_file(&canonical_rollout_path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(ThreadStoreError::Internal {
            message: format!(
                "failed to delete rollout file `{}`: {err}",
                canonical_rollout_path.display()
            ),
        }),
    }
}

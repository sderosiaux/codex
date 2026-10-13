use super::LocalThreadStore;
use super::helpers::owned_rollout_paths_from_index;
use super::helpers::restore_rollout_moves;
use super::helpers::rollout_path_is_archived;
use super::helpers::scoped_rollout_path;
use super::helpers::validated_rollout_file_name;
use crate::ArchiveThreadsParams;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use chrono::Utc;
use codex_rollout::RolloutReferenceIndex;
use tracing::warn;

use super::thread_rollout_resolver;
pub(super) async fn archive_threads(
    store: &LocalThreadStore,
    params: ArchiveThreadsParams,
) -> ThreadStoreResult<Vec<codex_protocol::ThreadId>> {
    let thread_ids = params.thread_ids;
    if thread_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut lock_thread_ids = params.writer_lock_thread_ids;
    lock_thread_ids.extend(thread_ids.iter().copied());
    lock_thread_ids.sort_unstable_by_key(ToString::to_string);
    lock_thread_ids.dedup();
    let mut _lifecycle_guards = Vec::with_capacity(lock_thread_ids.len());
    for thread_id in &lock_thread_ids {
        _lifecycle_guards.push(store.live_writer_locks.lock_lifecycle(*thread_id).await);
    }
    let mut _live_writer_guards = Vec::with_capacity(lock_thread_ids.len());
    for thread_id in &lock_thread_ids {
        _live_writer_guards.push(store.live_writer_locks.lock(*thread_id).await);
        if store.live_recorders.lock().await.contains_key(thread_id) {
            return Err(ThreadStoreError::Conflict {
                message: format!("thread {thread_id} already has an active writer"),
            });
        }
    }
    let _writer_guards = store.acquire_writer_locks(&lock_thread_ids).await?;
    // Only inspect active files whose names belong to the threads being archived.
    let reference_index = RolloutReferenceIndex::scan_unarchived_threads(
        store.config.codex_home.as_path(),
        &thread_ids,
    )
    .await
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to scan thread rollout files: {err}"),
    })?;

    let parent_thread_id = thread_ids[0];
    let mut archived_thread_ids = Vec::new();
    for thread_id in thread_ids {
        let rollout_paths = owned_rollout_paths_from_index(&reference_index, thread_id);
        match archive_thread_with_paths(store, thread_id, rollout_paths).await {
            Ok(()) => archived_thread_ids.push(thread_id),
            Err(err) if archived_thread_ids.is_empty() => return Err(err),
            Err(err) => warn!(
                "failed to archive spawned descendant thread {thread_id} while archiving {parent_thread_id}: {err}"
            ),
        }
    }
    Ok(archived_thread_ids)
}

async fn archive_thread_with_paths(
    store: &LocalThreadStore,
    thread_id: codex_protocol::ThreadId,
    mut rollout_paths: Vec<std::path::PathBuf>,
) -> ThreadStoreResult<()> {
    let state_db_ctx = store.state_db().await;
    let selected_rollout_path = thread_rollout_resolver::resolve_current(store, thread_id)
        .await?
        .map(|resolved| resolved.path)
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: format!("no rollout found for thread id {thread_id}"),
        })?;

    let archive_folder = store
        .config
        .codex_home
        .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR);
    std::fs::create_dir_all(&archive_folder).map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to archive thread: {err}"),
    })?;
    if !rollout_paths.contains(&selected_rollout_path) {
        rollout_paths.push(selected_rollout_path.clone());
    }
    let mut archived_path = None;
    let mut rollout_moves = Vec::new();
    for rollout_path in rollout_paths {
        if rollout_path_is_archived(store.config.codex_home.as_path(), rollout_path.as_path()) {
            continue;
        }
        let canonical_rollout_path = scoped_rollout_path(
            store.config.codex_home.join(codex_rollout::SESSIONS_SUBDIR),
            rollout_path.as_path(),
            "sessions",
        )?;
        let file_name =
            validated_rollout_file_name(canonical_rollout_path.as_path(), rollout_path.as_path())?;
        let destination = archive_folder.join(&file_name);
        if rollout_path == selected_rollout_path {
            archived_path = Some(destination.clone());
        }
        if !rollout_moves
            .iter()
            .any(|(source, _)| source == &canonical_rollout_path)
        {
            rollout_moves.push((canonical_rollout_path, destination));
        }
    }
    let archived_path = archived_path.ok_or_else(|| ThreadStoreError::Internal {
        message: format!("failed to archive selected rollout for thread {thread_id}"),
    })?;

    for (index, (source, destination)) in rollout_moves.iter().enumerate() {
        if let Err(err) = std::fs::rename(source, destination) {
            if let Err(restore_err) = restore_rollout_moves(&rollout_moves[..index]) {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "failed to archive thread: {err}; failed to restore moved rollouts: {restore_err}"
                    ),
                });
            }
            return Err(ThreadStoreError::Internal {
                message: format!("failed to archive thread: {err}"),
            });
        }
    }

    if let Some(ctx) = state_db_ctx
        && let Err(err) = ctx
            .mark_archived(thread_id, archived_path.as_path(), Utc::now())
            .await
    {
        if let Err(restore_err) = restore_rollout_moves(&rollout_moves) {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to update archived thread metadata: {err}; failed to restore moved rollouts: {restore_err}"
                ),
            });
        }
        return Err(ThreadStoreError::Internal {
            message: format!("failed to update archived thread metadata: {err}"),
        });
    }
    Ok(())
}

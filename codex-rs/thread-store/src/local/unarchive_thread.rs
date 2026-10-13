use super::LocalThreadStore;
use super::helpers::owned_rollout_paths;
use super::helpers::restore_rollout_moves;
use super::helpers::rollout_path_is_archived;
use super::helpers::scoped_rollout_path;
use super::helpers::touch_modified_time;
use super::helpers::validated_rollout_file_name;
use crate::ArchiveThreadParams;
use crate::ReadThreadParams;
use crate::StoredThread;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use codex_rollout::rollout_date_parts;

use super::thread_rollout_resolver;
use super::thread_rollout_resolver::RolloutLocation;

pub(super) async fn unarchive_thread(
    store: &LocalThreadStore,
    params: ArchiveThreadParams,
) -> ThreadStoreResult<StoredThread> {
    let thread_id = params.thread_id;
    let _lifecycle_guard = store.live_writer_locks.lock_lifecycle(thread_id).await;
    // Archive, delete, and revert use the same cross-process lock while moving or selecting
    // rollout files. Unarchive must participate before it moves those files back.
    let _writer_lock = store.acquire_writer_lock(thread_id)?;
    let state_db_ctx = store.state_db().await;
    let selected_archived_path =
        thread_rollout_resolver::resolve_current_including_archived(store, thread_id)
            .await?
            .filter(|resolved| resolved.location == RolloutLocation::Archived)
            .map(|resolved| resolved.path)
            .ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("no archived rollout found for thread id {thread_id}"),
            })?;

    let mut rollout_paths = owned_rollout_paths(store, thread_id).await?;
    if !rollout_paths.contains(&selected_archived_path) {
        rollout_paths.push(selected_archived_path.clone());
    }
    let mut restored_path = None;
    let mut rollout_moves = Vec::new();
    for rollout_path in rollout_paths {
        if !rollout_path_is_archived(store.config.codex_home.as_path(), rollout_path.as_path()) {
            continue;
        }
        let canonical_archived_path = scoped_rollout_path(
            store
                .config
                .codex_home
                .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
            rollout_path.as_path(),
            "archived",
        )?;
        let file_name =
            validated_rollout_file_name(canonical_archived_path.as_path(), rollout_path.as_path())?;
        let Some((year, month, day)) = rollout_date_parts(&file_name) else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "rollout path `{}` missing filename timestamp",
                    rollout_path.display()
                ),
            });
        };
        let dest_dir = store
            .config
            .codex_home
            .join(codex_rollout::SESSIONS_SUBDIR)
            .join(year)
            .join(month)
            .join(day);
        std::fs::create_dir_all(&dest_dir).map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to unarchive thread: {err}"),
        })?;
        let destination = dest_dir.join(&file_name);
        if rollout_path == selected_archived_path {
            restored_path = Some(destination.clone());
        }
        if !rollout_moves
            .iter()
            .any(|(source, _)| source == &canonical_archived_path)
        {
            rollout_moves.push((canonical_archived_path, destination));
        }
    }
    let restored_path = restored_path.ok_or_else(|| ThreadStoreError::Internal {
        message: format!("failed to unarchive selected rollout for thread {thread_id}"),
    })?;

    for (index, (source, destination)) in rollout_moves.iter().enumerate() {
        if let Err(err) = std::fs::rename(source, destination) {
            if let Err(restore_err) = restore_rollout_moves(&rollout_moves[..index]) {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "failed to unarchive thread: {err}; failed to restore moved rollouts: {restore_err}"
                    ),
                });
            }
            return Err(ThreadStoreError::Internal {
                message: format!("failed to unarchive thread: {err}"),
            });
        }
    }
    if let Err(err) = touch_modified_time(restored_path.as_path()) {
        if let Err(restore_err) = restore_rollout_moves(&rollout_moves) {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to update unarchived thread timestamp: {err}; failed to restore moved rollouts: {restore_err}"
                ),
            });
        }
        return Err(ThreadStoreError::Internal {
            message: format!("failed to update unarchived thread timestamp: {err}"),
        });
    }

    if let Some(ctx) = state_db_ctx
        && let Err(err) = ctx
            .mark_unarchived(thread_id, restored_path.as_path())
            .await
    {
        if let Err(restore_err) = restore_rollout_moves(&rollout_moves) {
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to update unarchived thread metadata: {err}; failed to restore moved rollouts: {restore_err}"
                ),
            });
        }
        return Err(ThreadStoreError::Internal {
            message: format!("failed to update unarchived thread metadata: {err}"),
        });
    }

    super::read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived: false,
            include_history: false,
        },
    )
    .await
}

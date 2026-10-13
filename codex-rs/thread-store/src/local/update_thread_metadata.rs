use std::path::Path;
use std::path::PathBuf;

use chrono::Utc;
use codex_protocol::SanitizedGitUrl;
use codex_protocol::ThreadId;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use codex_rollout::append_thread_name;
use codex_rollout::read_session_meta_line;
use codex_state::ThreadMetadataBuilder;
use tracing::warn;

use super::LocalThreadStore;
use super::helpers::git_info_from_parts;
use super::live_writer;
use super::pending_thread_metadata;
use super::thread_rollout_resolver;
use super::thread_rollout_resolver::ResolvedThreadRollout;
use super::thread_rollout_resolver::RolloutLocation;
use crate::GitInfoPatch;
use crate::ReadThreadParams;
use crate::StoredThread;
use crate::ThreadMetadataPatch;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::UpdateThreadMetadataParams;
use crate::local::read_thread;

pub(super) async fn update_thread_metadata(
    store: &LocalThreadStore,
    params: UpdateThreadMetadataParams,
) -> ThreadStoreResult<StoredThread> {
    let thread_id = params.thread_id;
    let mut pending_metadata = store.pending_thread_metadata.lock(thread_id).await;
    let pending_patch = pending_metadata
        .as_ref()
        .and_then(|metadata| metadata.as_ref().cloned());
    let mut patch = params.patch;
    if let Some(staged_patch) = pending_patch.as_ref() {
        let mut merged_patch = staged_patch.clone();
        merged_patch.merge(patch);
        patch = merged_patch;
    }
    if patch.project_id.is_some() && store.state_db().await.is_none() {
        return Err(ThreadStoreError::Unsupported {
            operation: "projects",
        });
    }
    if patch.is_empty() {
        return read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id,
                include_archived: params.include_archived,
                include_history: false,
            },
        )
        .await;
    }

    let staged_requires_rollout_compat = pending_patch
        .as_ref()
        .is_some_and(|patch| patch.memory_mode.is_some() || patch.git_info.is_some());
    let requires_rollout_compat =
        staged_requires_rollout_compat || requires_rollout_compatibility_update(&patch);
    let has_explicit_metadata = patch.name.is_some() || requires_rollout_compat;
    let history_mode = if has_explicit_metadata {
        match live_writer::live_writer_parts(store, thread_id).await {
            Ok((_recorder, _rollout_id, history_mode)) => Some(history_mode),
            Err(ThreadStoreError::ThreadNotFound { .. }) => Some(
                read_thread::read_thread(
                    store,
                    ReadThreadParams {
                        thread_id,
                        include_archived: params.include_archived,
                        include_history: false,
                    },
                )
                .await?
                .history_mode,
            ),
            Err(err) => return Err(err),
        }
    } else {
        None
    };
    let paginated = matches!(history_mode, Some(ThreadHistoryMode::Paginated));
    if paginated
        && patch.name.is_some()
        && live_writer::rollout_path(store, thread_id).await.is_ok()
    {
        // Naming saves a new thread even before its first turn. Persist its live recorder before
        // updating SQLite so the named thread can be resumed immediately or after a restart.
        live_writer::persist_thread(store, thread_id).await?;
    }
    let needs_rollout_compat = requires_rollout_compat || patch.name.is_some();
    // Reject competing writers before committing any part of a legacy rollout patch to SQLite.
    let writer_lock = if !paginated
        && needs_rollout_compat
        && (patch.memory_mode.is_some() || patch.git_info.is_some())
    {
        let live = store
            .live_recorders
            .lock()
            .await
            .get(&thread_id)
            .map(|entry| entry.writer_lock.clone());
        Some(match live {
            Some(guard) => guard,
            None => store.acquire_writer_lock(thread_id)?,
        })
    } else {
        None
    };
    let require_sqlite_write =
        pending_patch.is_some() || sqlite_write_failure_should_block(&patch) || paginated;
    let mut updated = apply_metadata_update(
        store,
        thread_id,
        patch.clone(),
        params.include_archived,
        require_sqlite_write,
        history_mode,
    )
    .await?;
    if paginated
        && requires_rollout_compat
        && let Some(git_info) = patch.git_info.as_ref()
    {
        // The generic upsert preserves non-null Git fields for rollout reconciliation. Apply the
        // explicit patch afterward so clears are written to SQLite too.
        let Some(state_db) = store.state_db().await else {
            return Err(ThreadStoreError::Internal {
                message: format!("sqlite state db unavailable for thread {thread_id}"),
            });
        };
        apply_thread_git_info_patch(state_db.as_ref(), thread_id, git_info).await?;
        updated = read_thread::read_thread(
            store,
            ReadThreadParams {
                thread_id,
                include_archived: params.include_archived,
                include_history: false,
            },
        )
        .await?;
    }
    if paginated {
        // Paginated metadata lives in SQLite. Keep the name index update, then stop before the
        // legacy SessionMeta compatibility path below.
        if let Some(name) = patch.name.as_ref()
            && let Err(err) = append_thread_name(
                store.config.codex_home.as_path(),
                thread_id,
                name.as_deref().unwrap_or_default(),
            )
            .await
        {
            warn!("failed to index paginated thread name for {thread_id}: {err}");
        }
        if pending_patch.is_some() {
            remove_pending_thread_metadata(store, thread_id, &mut pending_metadata).await;
        }
        return Ok(updated);
    }
    if !needs_rollout_compat {
        if pending_patch.is_some() {
            remove_pending_thread_metadata(store, thread_id, &mut pending_metadata).await;
        }
        return Ok(updated);
    }

    if live_writer::rollout_path(store, thread_id).await.is_ok() {
        live_writer::persist_thread(store, thread_id).await?;
    }
    let mut resolved_rollout = if params.include_archived {
        thread_rollout_resolver::resolve_current_including_archived(store, thread_id).await?
    } else {
        thread_rollout_resolver::resolve_current(store, thread_id).await?
    }
    .ok_or_else(|| ThreadStoreError::InvalidRequest {
        message: format!("thread not found: {thread_id}"),
    })?;
    let name = patch.name;
    let git_info = patch.git_info;
    if let Some(memory_mode) = patch.memory_mode
        && let Some(writer_lock) = writer_lock.as_ref()
    {
        update_rollout_metadata(
            store,
            thread_id,
            &resolved_rollout.path,
            writer_lock,
            |meta| {
                // Replay retains prior git metadata when this marker omits it.
                meta.git = None;
                meta.meta.memory_mode = Some(memory_mode_as_str(memory_mode).to_string());
            },
        )
        .await?;
        refresh_resolved_rollout_path(&mut resolved_rollout).await;
    }

    let state_db_ctx = store.state_db().await;
    codex_rollout::state_db::reconcile_rollout(
        state_db_ctx.as_deref(),
        resolved_rollout.path.as_path(),
        store.config.default_model_provider_id.as_str(),
        /*builder*/ None,
        &[],
        /*archived_only*/
        (resolved_rollout.location == RolloutLocation::Archived).then_some(true),
        /*new_thread_memory_mode*/ None,
    )
    .await;

    if let Some(name) = name {
        append_thread_name(
            store.config.codex_home.as_path(),
            thread_id,
            &name.unwrap_or_default(),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to index thread name: {err}"),
        })?;
    }

    let resolved_git_info = match git_info {
        Some(git_info) => {
            let Some(state_db) = store.state_db().await else {
                return Err(ThreadStoreError::Internal {
                    message: format!("sqlite state db unavailable for thread {thread_id}"),
                });
            };
            let metadata =
                state_db
                    .get_thread(thread_id)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to read git metadata for thread {thread_id}: {err}"
                        ),
                    })?;
            let Some(metadata) = metadata else {
                return Err(ThreadStoreError::Internal {
                    message: format!("thread metadata unavailable before git update: {thread_id}"),
                });
            };
            let memory_mode = state_db
                .get_thread_memory_mode(thread_id)
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to read memory mode for thread {thread_id}: {err}"),
                })?;
            let existing_git_info = git_info_from_parts(
                metadata.git_sha,
                metadata.git_branch,
                metadata.git_origin_url,
            );
            Some((
                resolve_git_info_patch(existing_git_info, git_info),
                memory_mode,
            ))
        }
        None => None,
    };
    if let Some(((sha, branch, origin_url), memory_mode)) = resolved_git_info.as_ref()
        && let Some(writer_lock) = writer_lock.as_ref()
    {
        update_rollout_metadata(
            store,
            thread_id,
            &resolved_rollout.path,
            writer_lock,
            |meta| {
                meta.git = Some(GitInfo {
                    commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
                    branch: branch.clone(),
                    repository_url: origin_url.clone(),
                });
                meta.meta.memory_mode = memory_mode.clone();
            },
        )
        .await?;
        refresh_resolved_rollout_path(&mut resolved_rollout).await;
        apply_thread_git_info(store, thread_id, sha, branch, origin_url).await?;
    }

    let mut thread = match read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived: params.include_archived,
            include_history: false,
        },
    )
    .await
    {
        Ok(thread) => thread,
        Err(_) => {
            read_thread::read_thread_by_rollout_path(
                store,
                resolved_rollout.path,
                params.include_archived,
                /*include_history*/ false,
            )
            .await?
        }
    };
    if let Some(((sha, branch, origin_url), _memory_mode)) = resolved_git_info {
        thread.git_info = git_info_from_parts(sha, branch, origin_url);
    }
    if pending_patch.is_some() {
        remove_pending_thread_metadata(store, thread_id, &mut pending_metadata).await;
    }
    Ok(thread)
}

async fn remove_pending_thread_metadata(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    pending_metadata: &mut Option<pending_thread_metadata::LockedPendingThreadMetadata>,
) {
    if let Some(mut metadata) = pending_metadata.take() {
        *metadata = None;
        drop(metadata);
        store.pending_thread_metadata.remove(thread_id).await;
    }
}

async fn refresh_resolved_rollout_path(resolved: &mut ResolvedThreadRollout) {
    if let Some(path) = codex_rollout::existing_rollout_path(resolved.path.as_path()).await {
        resolved.path = path;
    }
}

async fn apply_metadata_update(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    patch: ThreadMetadataPatch,
    include_archived: bool,
    require_sqlite_write: bool,
    history_mode: Option<ThreadHistoryMode>,
) -> ThreadStoreResult<StoredThread> {
    let live_rollout_path = live_writer::rollout_path(store, thread_id).await.ok();
    let mut rollout_path = patch.rollout_path.clone().or(live_rollout_path);
    let mut rollout_path_archived = rollout_path
        .as_deref()
        .is_some_and(|path| rollout_path_is_archived(store, path));
    let state_db = store.state_db().await;
    let sqlite_write_result: ThreadStoreResult<()> = if let Some(state_db) = state_db.as_ref() {
        let patch = patch.clone();
        async {
            let existing =
                state_db
                    .get_thread(thread_id)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!("failed to read thread metadata for {thread_id}: {err}"),
                    })?;
            // Existing timestamp-only observations must not rewrite metadata or its indexes.
            // Missing rows and changed rollout paths still need the normal repair path.
            if let Some(updated_at) = patch.updated_at
                && patch.is_empty_except_updated_at()
                && existing.as_ref().is_some_and(|metadata| {
                    rollout_path.as_ref().is_none_or(|path| path == &metadata.rollout_path)
                })
                && state_db
                    .touch_thread_updated_at(thread_id, updated_at)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!("failed to update thread timestamp for {thread_id}: {err}"),
                    })?
            {
                return Ok(());
            }
            let project_id = if existing.is_none()
                && let Some(Some(project_id)) = patch.project_id.as_ref()
                && state_db
                    .get_project(project_id)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to read initial thread project for {thread_id}: {err}"
                        ),
                    })?
                    .is_none()
            {
                Some(None)
            } else {
                patch.project_id.clone()
            };
            let advance_recency_at = patch.advance_recency_at;
            if existing.is_none() && rollout_path.is_none() {
                let resolved = if include_archived {
                    thread_rollout_resolver::resolve_current_including_archived(store, thread_id)
                        .await?
                } else {
                    thread_rollout_resolver::resolve_current(store, thread_id).await?
                }
                .ok_or_else(|| ThreadStoreError::InvalidRequest {
                    message: format!("thread not found: {thread_id}"),
                })?;
                rollout_path_archived = resolved.location == RolloutLocation::Archived;
                rollout_path = Some(resolved.path);
            }
            let mut metadata = match existing.clone() {
                Some(metadata) => metadata,
                None => {
                    let rollout_path =
                        rollout_path
                            .as_deref()
                            .ok_or_else(|| ThreadStoreError::Internal {
                                message: format!(
                                    "thread metadata missing rollout path for {thread_id}"
                                ),
                            })?;
                    metadata_for_missing_sqlite_row(
                        store,
                        thread_id,
                        rollout_path,
                        rollout_path_archived,
                        &patch,
                    )
                    .await?
                }
            };
            if let Some(rollout_path) = rollout_path {
                metadata.rollout_path = rollout_path;
            }
            if let Some(history_mode) = history_mode {
                // The read above gets the canonical mode from the rollout. Persist it before an
                // explicit paginated patch makes SQLite metadata authoritative.
                metadata.history_mode = history_mode;
            }
            if let Some(preview) = patch.preview {
                metadata.preview = Some(preview);
            }
            if let Some(title) = patch.title {
                metadata.title = title;
            }
            if let Some(model_provider) = patch.model_provider {
                metadata.model_provider = model_provider;
            }
            if let Some(model) = patch.model {
                metadata.model = Some(model);
            }
            if let Some(reasoning_effort) = patch.reasoning_effort {
                metadata.reasoning_effort = reasoning_effort;
            }
            if let Some(created_at) = patch.created_at {
                metadata.created_at = created_at;
            }
            if let Some(updated_at) = patch.updated_at {
                metadata.updated_at = updated_at;
            }
            if existing.is_none()
                && let Some(recency_at) = advance_recency_at
            {
                metadata.recency_at = recency_at;
            }
            if let Some(source) = patch.source {
                metadata.source = enum_to_string(&source);
            }
            metadata.originator = metadata.originator.or(patch.originator);
            metadata.creator_user_id = metadata.creator_user_id.or(patch.creator_user_id);
            metadata.creator_account_id = metadata.creator_account_id.or(patch.creator_account_id);
            if let Some(thread_source) = patch.thread_source {
                metadata.thread_source = thread_source;
            }
            if let Some(agent_nickname) = patch.agent_nickname {
                metadata.agent_nickname = agent_nickname;
            }
            if let Some(agent_role) = patch.agent_role {
                metadata.agent_role = agent_role;
            }
            if let Some(agent_path) = patch.agent_path {
                metadata.agent_path = agent_path;
            }
            if let Some(cwd) = patch.cwd {
                metadata.cwd = normalize_cwd(cwd);
            }
            if let Some(cli_version) = patch.cli_version {
                metadata.cli_version = cli_version;
            }
            if let Some(token_usage) = patch.token_usage {
                metadata.tokens_used = token_usage.total_tokens.max(0);
            }
            if let Some(first_user_message) = patch.first_user_message {
                metadata.first_user_message = Some(first_user_message);
            }
            if let Some(git_info) = patch.git_info {
                let existing_git_info = git_info_from_parts(
                    metadata.git_sha.clone(),
                    metadata.git_branch.clone(),
                    metadata.git_origin_url.clone(),
                );
                let (sha, branch, origin_url) = resolve_git_info_patch(existing_git_info, git_info);
                metadata.git_sha = sha;
                metadata.git_branch = branch;
                metadata.git_origin_url = origin_url;
            }
            if let Some(project_id) = project_id.as_ref() {
                metadata.project_id = project_id.clone();
            }
            if let Some(daybreak_enabled) = patch.daybreak_enabled {
                metadata.daybreak_enabled = Some(daybreak_enabled);
            }
            let upsert_result = state_db.upsert_thread(&metadata).await;
            if existing.is_none()
                && metadata.project_id.is_some()
                && matches!(&upsert_result, Err(err) if err.to_string().contains("FOREIGN KEY constraint failed"))
            {
                metadata.project_id = None;
                state_db.upsert_thread(&metadata).await.map_err(|err| {
                    ThreadStoreError::Internal {
                        message: format!("failed to update thread metadata for {thread_id}: {err}"),
                    }
                })?;
            } else {
                upsert_result.map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to update thread metadata for {thread_id}: {err}"),
                })?;
            }
            if existing.is_some()
                && let Some(project_id) = project_id.as_ref()
            {
                state_db
                    .set_thread_project(&thread_id.to_string(), project_id.as_deref())
                    .await
                    .map_err(|err| {
                        let message = err.to_string();
                        if message.contains("project not found") {
                            ThreadStoreError::InvalidRequest { message }
                        } else {
                            ThreadStoreError::Internal {
                                message: format!(
                                    "failed to update thread project for {thread_id}: {err}"
                                ),
                            }
                        }
                    })?
                    .ok_or_else(|| ThreadStoreError::Internal {
                        message: format!(
                            "thread metadata unavailable before project update: {thread_id}"
                        ),
                    })?;
            }
            if let Some(name) = patch.name.as_ref() {
                let history_mode = history_mode.ok_or_else(|| ThreadStoreError::Internal {
                    message: format!(
                        "thread history mode unavailable before name update: {thread_id}"
                    ),
                })?;
                let updated = match history_mode {
                    ThreadHistoryMode::Legacy => {
                        state_db
                            .update_thread_title(thread_id, name.as_deref().unwrap_or_default())
                            .await
                    }
                    ThreadHistoryMode::Paginated => {
                        state_db
                            .update_thread_name(thread_id, name.as_deref())
                            .await
                    }
                }
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!("failed to set thread name: {err}"),
                })?;
                if !updated {
                    return Err(ThreadStoreError::Internal {
                        message: format!(
                            "thread metadata unavailable before name update: {thread_id}"
                        ),
                    });
                }
            }
            if existing.is_some()
                && let Some(recency_at) = advance_recency_at
            {
                state_db
                    .touch_thread_recency_at(thread_id, recency_at)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to advance thread recency_at for {thread_id}: {err}"
                        ),
                    })?;
            }
            if let Some(memory_mode) = patch.memory_mode {
                state_db
                    .set_thread_memory_mode(thread_id, memory_mode_as_str(memory_mode))
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!("failed to update memory mode for {thread_id}: {err}"),
                    })?;
            }
            if let Some(daybreak_enabled) = patch.daybreak_enabled
                && !state_db
                    .set_thread_daybreak_enabled(thread_id, daybreak_enabled)
                    .await
                    .map_err(|err| ThreadStoreError::Internal {
                        message: format!(
                            "failed to update Daybreak preference for {thread_id}: {err}"
                        ),
                    })?
            {
                return Err(ThreadStoreError::Internal {
                    message: format!(
                        "thread metadata unavailable before Daybreak update: {thread_id}"
                    ),
                });
            }
            Ok(())
        }
        .await
    } else if require_sqlite_write {
        Err(ThreadStoreError::Internal {
            message: format!("sqlite state db unavailable for thread {thread_id}"),
        })
    } else {
        Ok(())
    };
    match sqlite_write_result {
        Ok(()) => {}
        Err(err) if require_sqlite_write || !sqlite_write_error_is_best_effort(&err) => {
            return Err(err);
        }
        Err(err) => {
            warn!("state db update_thread_metadata failed for {thread_id}: {err}");
        }
    }

    read_thread::read_thread(
        store,
        ReadThreadParams {
            thread_id,
            include_archived,
            include_history: false,
        },
    )
    .await
}

async fn metadata_for_missing_sqlite_row(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
    rollout_path_archived: bool,
    patch: &ThreadMetadataPatch,
) -> ThreadStoreResult<codex_state::ThreadMetadata> {
    let created_at = patch
        .created_at
        .or(patch.updated_at)
        .unwrap_or_else(Utc::now);
    let mut builder = ThreadMetadataBuilder::new(
        thread_id,
        rollout_path.to_path_buf(),
        created_at,
        patch.source.clone().unwrap_or(SessionSource::Unknown),
    );
    builder.model_provider = patch.model_provider.clone();
    builder.history_mode = canonical_history_mode(store, thread_id, rollout_path).await?;
    builder.thread_source = patch.thread_source.clone().flatten();
    builder.agent_nickname = patch.agent_nickname.clone().flatten();
    builder.agent_role = patch.agent_role.clone().flatten();
    builder.agent_path = patch.agent_path.clone().flatten();
    builder.cwd = patch.cwd.clone().map(normalize_cwd).unwrap_or_default();
    builder.cli_version = patch.cli_version.clone();
    let mut metadata = builder.build(store.config.default_model_provider_id.as_str());
    if rollout_path_archived {
        metadata.archived_at = Some(metadata.updated_at);
    }
    Ok(metadata)
}

async fn canonical_history_mode(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<ThreadHistoryMode> {
    let session_meta = match read_session_meta_line(rollout_path).await {
        Ok(session_meta) => session_meta,
        Err(err) => {
            if codex_rollout::existing_rollout_path(rollout_path)
                .await
                .is_none()
                && let Some(history_mode) = store
                    .live_recorders
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|entry| entry.history_mode)
            {
                // The live writer retains the canonical mode selected before its deferred
                // SessionMeta reaches JSONL.
                return Ok(history_mode);
            }
            return Err(ThreadStoreError::Internal {
                message: format!(
                    "failed to read canonical session metadata for {thread_id}: {err}"
                ),
            });
        }
    };
    if session_meta.meta.id != thread_id {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "failed to rebuild thread metadata: rollout session metadata id mismatch: expected {thread_id}, found {}",
                session_meta.meta.id
            ),
        });
    }
    Ok(session_meta.meta.history_mode)
}

fn requires_rollout_compatibility_update(patch: &ThreadMetadataPatch) -> bool {
    if patch.memory_mode.is_none() && patch.git_info.is_none() {
        return false;
    }
    !has_observed_metadata_facts(patch)
}

fn sqlite_write_failure_should_block(patch: &ThreadMetadataPatch) -> bool {
    // Before live metadata sync moved above the rollout writer, SQLite sync failures for
    // transcript-derived metadata, thread names, and memory-mode indexing were log-only. Keep that
    // failure isolation so a corrupted optional state DB does not make JSONL transcript durability
    // look broken. Explicit git-only updates still require SQLite because partial git patches need
    // the existing SQLite value to preserve unspecified fields. Project and Daybreak updates
    // require SQLite because those preferences only exist in the state database.
    patch.project_id.is_some()
        || patch.daybreak_enabled.is_some()
        || (patch.git_info.is_some() && !has_observed_metadata_facts(patch))
}

fn sqlite_write_error_is_best_effort(err: &ThreadStoreError) -> bool {
    matches!(err, ThreadStoreError::Internal { .. })
}

fn has_observed_metadata_facts(patch: &ThreadMetadataPatch) -> bool {
    patch.rollout_path.is_some()
        || patch.preview.is_some()
        || patch.title.is_some()
        || patch.model_provider.is_some()
        || patch.model.is_some()
        || patch.reasoning_effort.is_some()
        || patch.created_at.is_some()
        || patch.source.is_some()
        || patch.originator.is_some()
        || patch.creator_user_id.is_some()
        || patch.creator_account_id.is_some()
        || patch.thread_source.is_some()
        || patch.agent_nickname.is_some()
        || patch.agent_role.is_some()
        || patch.agent_path.is_some()
        || patch.cwd.is_some()
        || patch.cli_version.is_some()
        || patch.token_usage.is_some()
        || patch.first_user_message.is_some()
}

fn enum_to_string<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(value)) => value,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn normalize_cwd(cwd: PathBuf) -> PathBuf {
    codex_utils_path::normalize_for_path_comparison(cwd.as_path()).unwrap_or(cwd)
}

async fn apply_thread_git_info_patch(
    state_db: &codex_state::StateRuntime,
    thread_id: ThreadId,
    git_info: &GitInfoPatch,
) -> ThreadStoreResult<()> {
    let updated = state_db
        .update_thread_git_info(
            thread_id,
            git_info.sha.as_ref().map(|sha| sha.as_deref()),
            git_info.branch.as_ref().map(|branch| branch.as_deref()),
            git_info
                .origin_url
                .as_ref()
                .map(|origin_url| origin_url.as_ref()),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to update git metadata for thread {thread_id}: {err}"),
        })?;
    if updated {
        Ok(())
    } else {
        Err(ThreadStoreError::Internal {
            message: format!("thread metadata unavailable before git update: {thread_id}"),
        })
    }
}

async fn apply_thread_git_info(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    sha: &Option<String>,
    branch: &Option<String>,
    origin_url: &Option<SanitizedGitUrl>,
) -> ThreadStoreResult<()> {
    let Some(state_db) = store.state_db().await else {
        return Err(ThreadStoreError::Internal {
            message: format!("sqlite state db unavailable for thread {thread_id}"),
        });
    };
    let updated = state_db
        .update_thread_git_info(
            thread_id,
            Some(sha.as_deref()),
            Some(branch.as_deref()),
            Some(origin_url.as_ref()),
        )
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to update git metadata for thread {thread_id}: {err}"),
        })?;
    if updated {
        Ok(())
    } else {
        Err(ThreadStoreError::Internal {
            message: format!("thread metadata disappeared before update completed: {thread_id}"),
        })
    }
}

fn resolve_git_info_patch(
    existing: Option<GitInfo>,
    git_info: GitInfoPatch,
) -> (Option<String>, Option<String>, Option<SanitizedGitUrl>) {
    let (existing_sha, existing_branch, existing_origin_url) = match existing {
        Some(info) => (
            info.commit_hash.map(|sha| sha.0),
            info.branch,
            info.repository_url,
        ),
        None => (None, None, None),
    };
    let sha = git_info.sha.unwrap_or(existing_sha);
    let branch = git_info.branch.unwrap_or(existing_branch);
    let origin_url = git_info.origin_url.unwrap_or(existing_origin_url);
    (sha, branch, origin_url)
}

// Both metadata patches use the same owned recorder; temporary writers also survive cancellation.
async fn update_rollout_metadata(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    path: &Path,
    writer_lock: &super::WriterLockGuard,
    patch: impl FnOnce(&mut SessionMetaLine),
) -> ThreadStoreResult<()> {
    // Keep shutdown from closing the selected recorder before its metadata append is flushed.
    let _live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    let io_error = |err| ThreadStoreError::Internal {
        message: format!("failed to update rollout metadata: {err}"),
    };
    let recorder = store
        .live_recorders
        .lock()
        .await
        .get(&thread_id)
        .map(|entry| entry.recorder.clone());
    let mut metadata = codex_rollout::read_session_meta_line(path)
        .await
        .map_err(io_error)?;
    if metadata.meta.id != thread_id {
        return Err(ThreadStoreError::Internal {
            message: format!(
                "rollout session metadata id mismatch: expected {thread_id}, found {}",
                metadata.meta.id
            ),
        });
    }
    let temporary = recorder.is_none();
    let recorder = match recorder {
        Some(recorder) => recorder,
        None => RolloutRecorder::new_with_writer_lock(
            &RolloutConfig {
                codex_home: store.config.codex_home.clone(),
                sqlite: store.config.sqlite.clone(),
                cwd: metadata.meta.cwd.clone(),
                model_provider_id: store.config.default_model_provider_id.clone(),
                generate_memories: metadata.meta.memory_mode.as_deref() != Some("disabled"),
            },
            RolloutRecorderParams::resume(path.to_path_buf()),
            writer_lock.clone(),
        )
        .await
        .map_err(io_error)?,
    };
    patch(&mut metadata);
    recorder
        .record_canonical_items(&[RolloutItem::SessionMeta(metadata)])
        .await
        .map_err(io_error)?;
    recorder.flush().await.map_err(io_error)?;
    if temporary {
        recorder.shutdown().await.map_err(io_error)?;
    }
    Ok(())
}

fn memory_mode_as_str(mode: ThreadMemoryMode) -> &'static str {
    match mode {
        ThreadMemoryMode::Enabled => "enabled",
        ThreadMemoryMode::Disabled => "disabled",
    }
}

fn rollout_path_is_archived(store: &LocalThreadStore, path: &Path) -> bool {
    super::helpers::rollout_path_is_archived(store.config.codex_home.as_path(), path)
}

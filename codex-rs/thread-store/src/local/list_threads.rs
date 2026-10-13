use std::collections::HashMap;

use chrono::DateTime;
use chrono::Utc;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::parse_cursor;
use codex_state::ThreadFilterOptions;

use super::LocalThreadStore;
use super::helpers::resolve_thread_names;
use super::helpers::resolve_thread_section_metadata;
use super::helpers::set_thread_name;
use super::helpers::stored_thread_from_rollout_item;
use super::read_thread::stored_thread_from_state_metadata;
use crate::ListThreadsParams;
use crate::SortDirection;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn list_threads(
    store: &LocalThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<ThreadPage> {
    if params.sort_key == ThreadSortKey::SectionPosition {
        return list_section_threads(store, params).await;
    }
    let cursor = params
        .cursor
        .as_deref()
        .map(|cursor| {
            parse_cursor(cursor).ok_or_else(|| ThreadStoreError::InvalidRequest {
                message: format!("invalid cursor: {cursor}"),
            })
        })
        .transpose()?;
    let sort_key = match params.sort_key {
        ThreadSortKey::CreatedAt => codex_rollout::ThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt => codex_rollout::ThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt => codex_rollout::ThreadSortKey::RecencyAt,
        ThreadSortKey::SectionPosition => unreachable!("section order uses the state database"),
    };
    let sort_direction = match params.sort_direction {
        SortDirection::Asc => codex_rollout::SortDirection::Asc,
        SortDirection::Desc => codex_rollout::SortDirection::Desc,
    };
    let state_db = store.state_db().await;
    let rollout_config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite: store.config.sqlite.clone(),
        cwd: store.config.codex_home.clone(),
        model_provider_id: store.config.default_model_provider_id.clone(),
        generate_memories: false,
    };
    let page = list_rollout_threads(
        state_db.clone(),
        &rollout_config,
        store.config.default_model_provider_id.as_str(),
        &params,
        cursor.as_ref(),
        sort_key,
        sort_direction,
    )
    .await?;

    let next_cursor = page
        .next_cursor
        .as_ref()
        .and_then(|cursor| serde_json::to_value(cursor).ok())
        .and_then(|value| value.as_str().map(str::to_owned));
    let mut items = page
        .items
        .into_iter()
        .filter_map(|item| {
            stored_thread_from_rollout_item(
                item,
                params.archived,
                store.config.default_model_provider_id.as_str(),
            )
        })
        .collect::<Vec<_>>();

    let thread_history_modes = items
        .iter()
        .map(|thread| (thread.thread_id, thread.history_mode))
        .collect::<HashMap<_, _>>();
    let names =
        resolve_thread_names(store, &thread_history_modes, params.use_state_db_only).await?;
    for thread in &mut items {
        if let Some(name) = names.get(&thread.thread_id).cloned() {
            set_thread_name(thread, name);
        }
    }
    if let Some(state_db) = state_db {
        let sectioned_thread_ids = items
            .iter()
            .filter(|thread| thread.section.is_some())
            .map(|thread| thread.thread_id)
            .collect::<Vec<_>>();
        let section_metadata =
            resolve_thread_section_metadata(state_db.as_ref(), &sectioned_thread_ids).await;
        for thread in items.iter_mut().filter(|thread| thread.section.is_some()) {
            if let Some((section_position, section_entered_at)) =
                section_metadata.get(&thread.thread_id)
            {
                thread.section_position = *section_position;
                thread.section_entered_at = *section_entered_at;
            }
        }
    }

    if let Some(project_id) = params.project_id.as_ref() {
        items.retain(|thread| &thread.project_id == project_id);
    }

    Ok(ThreadPage { items, next_cursor })
}

async fn list_section_threads(
    store: &LocalThreadStore,
    params: ListThreadsParams,
) -> ThreadStoreResult<ThreadPage> {
    let section = params
        .section
        .as_ref()
        .and_then(Option::as_deref)
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: "section-position sorting requires a section filter".to_owned(),
        })?;
    let state_db = store
        .state_db()
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "state DB unavailable for section-ordered thread listing".to_owned(),
        })?;

    let anchor = params
        .cursor
        .as_deref()
        .map(|cursor| -> ThreadStoreResult<codex_state::Anchor> {
            let (position, thread_id) =
                cursor
                    .split_once('|')
                    .ok_or_else(|| ThreadStoreError::InvalidRequest {
                        message: format!("invalid cursor: {cursor}"),
                    })?;
            let timestamp = position
                .parse::<i64>()
                .ok()
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .ok_or_else(|| ThreadStoreError::InvalidRequest {
                    message: format!("invalid cursor: {cursor}"),
                })?;
            let thread_id = codex_protocol::ThreadId::from_string(thread_id).map_err(|_| {
                ThreadStoreError::InvalidRequest {
                    message: format!("invalid cursor: {cursor}"),
                }
            })?;
            Ok(codex_state::Anchor {
                ts: timestamp,
                id: Some(thread_id),
            })
        })
        .transpose()?;
    let allowed_sources = params
        .allowed_sources
        .iter()
        .map(|source| match serde_json::to_value(source) {
            Ok(serde_json::Value::String(source)) => source,
            Ok(source) => source.to_string(),
            Err(_) => String::new(),
        })
        .collect::<Vec<_>>();
    let normalized_cwd_filters = params.cwd_filters.as_ref().map(|filters| {
        filters
            .iter()
            .map(|cwd| codex_rollout::state_db::normalize_cwd_for_state_db(cwd))
            .collect::<Vec<_>>()
    });
    let filters = ThreadFilterOptions {
        archived_only: params.archived,
        allowed_sources: allowed_sources.as_slice(),
        model_providers: params.model_providers.as_deref(),
        cwd_filters: normalized_cwd_filters.as_deref(),
        section: Some(Some(section)),
        project_id: params
            .project_id
            .as_ref()
            .map(|project_id| project_id.as_deref()),
        anchor: anchor.as_ref(),
        sort_key: codex_state::SortKey::SectionPosition,
        sort_direction: match params.sort_direction {
            SortDirection::Asc => codex_state::SortDirection::Asc,
            SortDirection::Desc => codex_state::SortDirection::Desc,
        },
        search_term: params.search_term.as_deref(),
    };
    let page = match params.relation_filter {
        Some(ThreadRelationFilter::DirectChildrenOf(thread_id)) => {
            state_db
                .list_threads_by_relation(
                    params.page_size,
                    codex_state::ThreadRelationFilter::DirectChildrenOf(thread_id),
                    filters,
                )
                .await
        }
        Some(ThreadRelationFilter::DescendantsOf(thread_id)) => {
            state_db
                .list_threads_by_relation(
                    params.page_size,
                    codex_state::ThreadRelationFilter::DescendantsOf(thread_id),
                    filters,
                )
                .await
        }
        None => state_db.list_threads(params.page_size, filters).await,
    }
    .map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to list section-ordered threads: {err}"),
    })?;

    let codex_state::ThreadsPage {
        items: metadata_items,
        parent_thread_ids,
        next_anchor,
        ..
    } = page;
    let items = metadata_items
        .into_iter()
        .map(|metadata| {
            let parent_thread_id = parent_thread_ids.get(&metadata.id).copied();
            stored_thread_from_state_metadata(store, metadata, parent_thread_id)
        })
        .collect();
    let next_cursor = next_anchor.and_then(|anchor| {
        anchor.id.map(|thread_id| {
            let position = anchor.ts.timestamp_millis();
            format!("{position}|{thread_id}")
        })
    });
    Ok(ThreadPage { items, next_cursor })
}

pub(super) async fn list_rollout_threads(
    state_db: Option<codex_rollout::StateDbHandle>,
    config: &RolloutConfig,
    default_model_provider_id: &str,
    params: &ListThreadsParams,
    cursor: Option<&codex_rollout::Cursor>,
    sort_key: codex_rollout::ThreadSortKey,
    sort_direction: codex_rollout::SortDirection,
) -> ThreadStoreResult<codex_rollout::ThreadsPage> {
    if params.relation_filter.is_some() || params.section.is_some() || params.project_id.is_some() {
        let relation_filter = params
            .relation_filter
            .map(|relation_filter| match relation_filter {
                ThreadRelationFilter::DirectChildrenOf(parent_thread_id) => {
                    codex_state::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)
                }
                ThreadRelationFilter::DescendantsOf(ancestor_thread_id) => {
                    codex_state::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)
                }
            });
        let page = codex_rollout::state_db::list_threads_db(
            state_db.as_deref(),
            &config.sqlite,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            relation_filter,
            params.archived,
            params.section.as_ref().map(Option::as_deref),
            params.project_id.as_ref().map(Option::as_deref),
            params.search_term.as_deref(),
        )
        .await
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "state DB unavailable for filtered thread listing".to_string(),
        })?;
        return Ok(page.into());
    }

    let page = if params.use_state_db_only && params.archived {
        RolloutRecorder::list_archived_threads_from_state_db(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else if params.use_state_db_only {
        RolloutRecorder::list_threads_from_state_db(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else if params.archived {
        RolloutRecorder::list_archived_threads(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    } else {
        RolloutRecorder::list_threads(
            state_db,
            config,
            params.page_size,
            cursor,
            sort_key,
            sort_direction,
            params.allowed_sources.as_slice(),
            params.model_providers.as_deref(),
            params.cwd_filters.as_deref(),
            default_model_provider_id,
            params.search_term.as_deref(),
        )
        .await
    };
    page.map_err(|err| ThreadStoreError::Internal {
        message: format!("failed to list threads: {err}"),
    })
}

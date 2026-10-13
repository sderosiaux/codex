use super::*;
use crate::SortDirection;
use codex_protocol::SanitizedGitUrl;
use codex_protocol::protocol::SessionSource;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

// The `threads` schema is shared with upstream Codex installs and keeps these
// non-null columns; record what this build always does.
const SANDBOX_POLICY_COLUMN_VALUE: &str = "danger-full-access";
const APPROVAL_MODE_COLUMN_VALUE: &str = "never";

impl StateRuntime {
    pub async fn get_thread(&self, id: ThreadId) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let row = sqlx::query(
            r#"
SELECT
    threads.id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.originator,
    threads.creator_user_id,
    threads.creator_account_id,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.name,
    threads.preview,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.thread_section_id AS section,
    (
        SELECT thread_sections.name
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_name,
    (
        SELECT thread_sections.appearance
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_appearance,
    threads.section_position,
    threads.section_entered_at_ms,
    threads.project_id,
    threads.daybreak_enabled,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
FROM threads
WHERE threads.id = ?
            "#,
        )
        .bind(id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from))
            .transpose()
    }

    /// Permanently promote a thread, preserving its canonical name or a legacy-visible fallback.
    pub async fn mark_thread_paginated(
        &self,
        thread_id: ThreadId,
        legacy_name: Option<&str>,
    ) -> anyhow::Result<bool> {
        // Legacy threads display `title`, then fall back to the name index. Paginated threads
        // display `name`; `title` remains derived metadata used for search. Preserve an existing
        // `name`.
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    history_mode = 'paginated',
    name = CASE
        WHEN name IS NULL OR trim(name) = '' THEN ?
        ELSE name
    END
WHERE id = ?
            "#,
        )
        .bind(legacy_name)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }
    pub async fn get_thread_memory_mode(&self, id: ThreadId) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT memory_mode FROM threads WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(self.pool.as_ref())
            .await?;
        Ok(row.and_then(|row| row.try_get("memory_mode").ok()))
    }

    pub async fn set_thread_preview_if_empty(
        &self,
        thread_id: ThreadId,
        preview: &str,
    ) -> anyhow::Result<bool> {
        let preview = preview.trim();
        if preview.is_empty() {
            return Ok(false);
        }
        let result = sqlx::query(
            r#"
UPDATE threads
SET preview = ?
WHERE id = ? AND preview = ''
            "#,
        )
        .bind(preview)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Persist or replace the directional parent-child edge for a spawned thread.
    pub async fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO UPDATE SET
    parent_thread_id = excluded.parent_thread_id,
    status = excluded.status
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(status.as_ref())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Update the persisted lifecycle status of a spawned thread's incoming edge.
    pub async fn set_thread_spawn_edge_status(
        &self,
        child_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE thread_spawn_edges SET status = ? WHERE child_thread_id = ?")
            .bind(status.as_ref())
            .bind(child_thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// List direct spawned children of `parent_thread_id` whose edge matches `status`.
    pub async fn list_thread_spawn_children_with_status(
        &self,
        parent_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, Some(status))
            .await
    }

    /// List all direct spawned children of `parent_thread_id`.
    pub async fn list_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_children_matching(parent_thread_id, /*status*/ None)
            .await
    }

    /// List spawned descendants of `root_thread_id` whose edges match `status`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants_with_status(
        &self,
        root_thread_id: ThreadId,
        status: crate::DirectionalThreadSpawnEdgeStatus,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, Some(status))
            .await
    }

    /// List all spawned descendants of `root_thread_id`.
    ///
    /// Descendants are returned breadth-first by depth, then by thread id for stable ordering.
    pub async fn list_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> anyhow::Result<Vec<ThreadId>> {
        self.list_thread_spawn_descendants_matching(root_thread_id, /*status*/ None)
            .await
    }

    /// Find a direct spawned child of `parent_thread_id` by canonical agent path.
    pub async fn find_thread_spawn_child_by_path(
        &self,
        parent_thread_id: ThreadId,
        agent_path: &str,
    ) -> anyhow::Result<Option<ThreadId>> {
        let rows = sqlx::query(
            r#"
SELECT threads.id
FROM thread_spawn_edges
JOIN threads ON threads.id = thread_spawn_edges.child_thread_id
WHERE thread_spawn_edges.parent_thread_id = ?
  AND threads.agent_path = ?
ORDER BY threads.id
LIMIT 2
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(agent_path)
        .fetch_all(self.pool.as_ref())
        .await?;
        one_thread_id_from_rows(rows, agent_path)
    }

    /// Find a spawned descendant of `root_thread_id` by canonical agent path.
    pub async fn find_thread_spawn_descendant_by_path(
        &self,
        root_thread_id: ThreadId,
        agent_path: &str,
    ) -> anyhow::Result<Option<ThreadId>> {
        let rows = sqlx::query(
            r#"
WITH RECURSIVE subtree(child_thread_id) AS (
    SELECT child_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id = ?
    UNION ALL
    SELECT edge.child_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
)
SELECT threads.id
FROM subtree
JOIN threads ON threads.id = subtree.child_thread_id
WHERE threads.agent_path = ?
ORDER BY threads.id
LIMIT 2
            "#,
        )
        .bind(root_thread_id.to_string())
        .bind(agent_path)
        .fetch_all(self.pool.as_ref())
        .await?;
        one_thread_id_from_rows(rows, agent_path)
    }

    async fn list_thread_spawn_children_matching(
        &self,
        parent_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ",
        );
        builder.push_bind(parent_thread_id.to_string());
        if let Some(status) = status {
            builder.push(" AND status = ").push_bind(status.to_string());
        }
        builder.push(" ORDER BY child_thread_id");

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    async fn list_thread_spawn_descendants_matching(
        &self,
        root_thread_id: ThreadId,
        status: Option<crate::DirectionalThreadSpawnEdgeStatus>,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new(
            r#"
WITH RECURSIVE subtree(child_thread_id, depth) AS (
    SELECT child_thread_id, 1
    FROM thread_spawn_edges
    WHERE parent_thread_id =
            "#,
        );
        builder.push_bind(root_thread_id.to_string());
        if let Some(status) = status {
            let status = status.to_string();
            builder.push(" AND status = ").push_bind(status.clone());
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
    WHERE status =
                "#,
            );
            builder.push_bind(status);
        } else {
            builder.push(
                r#"
    UNION ALL
    SELECT edge.child_thread_id, subtree.depth + 1
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
                "#,
            );
        }
        builder.push(
            r#"
)
SELECT child_thread_id
FROM subtree
ORDER BY depth ASC, child_thread_id ASC
            "#,
        );

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                ThreadId::try_from(row.try_get::<String, _>("child_thread_id")?).map_err(Into::into)
            })
            .collect()
    }

    async fn insert_thread_spawn_edge_if_absent(
        &self,
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
    ) -> anyhow::Result<()> {
        sqlx::query(
            r#"
INSERT INTO thread_spawn_edges (
    parent_thread_id,
    child_thread_id,
    status
) VALUES (?, ?, ?)
ON CONFLICT(child_thread_id) DO NOTHING
            "#,
        )
        .bind(parent_thread_id.to_string())
        .bind(child_thread_id.to_string())
        .bind(crate::DirectionalThreadSpawnEdgeStatus::Open.as_ref())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn insert_thread_spawn_edge_from_source_if_absent(
        &self,
        child_thread_id: ThreadId,
        source: &str,
    ) -> anyhow::Result<()> {
        let Some(parent_thread_id) = thread_spawn_parent_thread_id_from_source_str(source) else {
            return Ok(());
        };
        self.insert_thread_spawn_edge_if_absent(parent_thread_id, child_thread_id)
            .await
    }

    /// Find a rollout path by thread id using the underlying database.
    pub async fn find_rollout_path_by_id(
        &self,
        id: ThreadId,
        archived_only: Option<bool>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT rollout_path FROM threads WHERE id = ");
        builder.push_bind(id.to_string());
        match archived_only {
            Some(true) => {
                builder.push(" AND archived = 1");
            }
            Some(false) => {
                builder.push(" AND archived = 0");
            }
            None => {}
        }
        let row = builder.build().fetch_optional(self.pool.as_ref()).await?;
        Ok(row
            .and_then(|r| r.try_get::<String, _>("rollout_path").ok())
            .map(PathBuf::from))
    }

    /// Swap one thread's rollout path only when it still matches the expected path.
    ///
    /// This intentionally updates only the physical path. The logical thread metadata remains
    /// attached to the stable thread id.
    pub async fn replace_rollout_path_if_current(
        &self,
        id: ThreadId,
        expected: &Path,
        replacement: &Path,
    ) -> anyhow::Result<bool> {
        let result =
            sqlx::query("UPDATE threads SET rollout_path = ? WHERE id = ? AND rollout_path = ?")
                .bind(replacement.display().to_string())
                .bind(id.to_string())
                .bind(expected.display().to_string())
                .execute(self.pool.as_ref())
                .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Find the newest thread whose user-facing title exactly matches `title`.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_thread_by_exact_title(
        &self,
        title: &str,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
        cwd: Option<&Path>,
    ) -> anyhow::Result<Option<crate::ThreadMetadata>> {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_thread_select_columns(&mut builder);
        builder.push(" FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                section: None,
                project_id: None,
                anchor: None,
                sort_key: crate::SortKey::UpdatedAt,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            /*include_thread_id_tiebreaker*/ false,
        );
        builder.push(" AND threads.title = ");
        builder.push_bind(title);
        if let Some(cwd) = cwd {
            builder.push(" AND threads.cwd = ");
            builder.push_bind(cwd.display().to_string());
        }
        push_thread_order_and_limit(
            &mut builder,
            crate::SortKey::UpdatedAt,
            SortDirection::Desc,
            OrderByIndex::Enabled,
            /*include_thread_id_tiebreaker*/ false,
            /*limit*/ 1,
        );

        let row = builder.build().fetch_optional(self.pool.as_ref()).await?;
        row.map(|row| ThreadRow::try_from_row(&row).and_then(crate::ThreadMetadata::try_from))
            .transpose()
    }

    /// List threads using the underlying database.
    pub async fn list_threads(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, /*relation_filter*/ None)
            .await
    }

    /// List direct children of `parent_thread_id` using persisted spawn edges.
    pub async fn list_threads_by_parent(
        &self,
        page_size: usize,
        parent_thread_id: ThreadId,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_by_relation(
            page_size,
            crate::ThreadRelationFilter::DirectChildrenOf(parent_thread_id),
            filters,
        )
        .await
    }

    /// List threads matching a persisted spawn-graph relationship.
    pub async fn list_threads_by_relation(
        &self,
        page_size: usize,
        relation_filter: crate::ThreadRelationFilter,
        filters: ThreadFilterOptions<'_>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        self.list_threads_matching(page_size, filters, Some(relation_filter))
            .await
    }

    async fn list_threads_matching(
        &self,
        page_size: usize,
        filters: ThreadFilterOptions<'_>,
        relation_filter: Option<crate::ThreadRelationFilter>,
    ) -> anyhow::Result<crate::ThreadsPage> {
        let limit = page_size.saturating_add(1);

        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_list_threads_query(&mut builder, filters, relation_filter, limit);

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        let mut items = Vec::with_capacity(rows.len());
        let mut parent_thread_ids = std::collections::HashMap::new();
        for row in rows {
            let item = ThreadRow::try_from_row(&row).and_then(ThreadMetadata::try_from)?;
            if relation_filter.is_some()
                && let Some(parent_thread_id) =
                    row.try_get::<Option<String>, _>("parent_thread_id")?
            {
                parent_thread_ids.insert(item.id, ThreadId::try_from(parent_thread_id)?);
            }
            items.push(item);
        }
        let num_scanned_rows = items.len();
        let next_anchor = if items.len() > page_size {
            if let Some(overflow_item) = items.pop() {
                parent_thread_ids.remove(&overflow_item.id);
            }
            items.last().and_then(|item| {
                anchor_from_item(item, filters.sort_key, relation_filter.is_some())
            })
        } else {
            None
        };
        Ok(ThreadsPage {
            items,
            parent_thread_ids,
            next_anchor,
            num_scanned_rows,
        })
    }

    /// List thread ids using the underlying database (no rollout scanning).
    pub async fn list_thread_ids(
        &self,
        limit: usize,
        anchor: Option<&crate::Anchor>,
        sort_key: crate::SortKey,
        allowed_sources: &[String],
        model_providers: Option<&[String]>,
        archived_only: bool,
    ) -> anyhow::Result<Vec<ThreadId>> {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT threads.id FROM threads");
        push_thread_filters(
            &mut builder,
            ThreadFilterOptions {
                archived_only,
                allowed_sources,
                model_providers,
                cwd_filters: None,
                section: None,
                project_id: None,
                anchor,
                sort_key,
                sort_direction: SortDirection::Desc,
                search_term: None,
            },
            matches!(
                sort_key,
                crate::SortKey::CreatedAt
                    | crate::SortKey::RecencyAt
                    | crate::SortKey::SectionPosition
            ),
        );
        push_thread_order_and_limit(
            &mut builder,
            sort_key,
            SortDirection::Desc,
            OrderByIndex::Enabled,
            matches!(
                sort_key,
                crate::SortKey::CreatedAt
                    | crate::SortKey::RecencyAt
                    | crate::SortKey::SectionPosition
            ),
            limit,
        );

        let rows = builder.build().fetch_all(self.pool.as_ref()).await?;
        rows.into_iter()
            .map(|row| {
                let id: String = row.try_get("id")?;
                Ok(ThreadId::try_from(id)?)
            })
            .collect()
    }

    /// Insert or replace thread metadata directly.
    pub async fn upsert_thread(&self, metadata: &crate::ThreadMetadata) -> anyhow::Result<()> {
        self.upsert_thread_with_creation_memory_mode(metadata, /*creation_memory_mode*/ None)
            .await
    }

    pub async fn insert_thread_if_absent(
        &self,
        metadata: &crate::ThreadMetadata,
    ) -> anyhow::Result<bool> {
        let updated_at = self.allocate_thread_updated_at(metadata.updated_at)?;
        let recency_at = self.allocate_thread_recency_at(metadata.recency_at)?;
        let preview = metadata_preview(metadata);
        let result = sqlx::query(
            r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    recency_at,
    created_at_ms,
    updated_at_ms,
    recency_at_ms,
    source,
    originator,
    creator_user_id,
    creator_account_id,
    history_mode,
    thread_source,
    agent_nickname,
    agent_role,
    agent_path,
    model_provider,
    model,
    reasoning_effort,
    cwd,
    cli_version,
    title,
    name,
    preview,
    sandbox_policy,
    approval_mode,
    tokens_used,
    first_user_message,
    archived,
    archived_at,
    thread_section_id,
    section_position,
    section_entered_at_ms,
    git_sha,
    git_branch,
    git_origin_url,
    memory_mode,
    project_id,
    daybreak_enabled
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO NOTHING
            "#,
        )
        .bind(metadata.id.to_string())
        .bind(metadata.rollout_path.display().to_string())
        .bind(datetime_to_epoch_seconds(metadata.created_at))
        .bind(datetime_to_epoch_seconds(updated_at))
        .bind(datetime_to_epoch_seconds(recency_at))
        .bind(datetime_to_epoch_millis(metadata.created_at))
        .bind(datetime_to_epoch_millis(updated_at))
        .bind(datetime_to_epoch_millis(recency_at))
        .bind(metadata.source.as_str())
        .bind(metadata.originator.as_deref())
        .bind(metadata.creator_user_id.as_deref())
        .bind(metadata.creator_account_id.as_deref())
        .bind(metadata.history_mode.as_str())
        .bind(
            metadata
                .thread_source
                .as_ref()
                .map(codex_protocol::protocol::ThreadSource::as_str),
        )
        .bind(metadata.agent_nickname.as_deref())
        .bind(metadata.agent_role.as_deref())
        .bind(metadata.agent_path.as_deref())
        .bind(metadata.model_provider.as_str())
        .bind(metadata.model.as_deref())
        .bind(
            metadata
                .reasoning_effort
                .as_ref()
                .map(crate::extract::enum_to_string),
        )
        .bind(metadata.cwd.display().to_string())
        .bind(metadata.cli_version.as_str())
        .bind(metadata.title.as_str())
        .bind(metadata.name.as_deref())
        .bind(preview)
        .bind(SANDBOX_POLICY_COLUMN_VALUE)
        .bind(APPROVAL_MODE_COLUMN_VALUE)
        .bind(metadata.tokens_used)
        .bind(metadata.first_user_message.as_deref().unwrap_or_default())
        .bind(metadata.archived_at.is_some())
        .bind(metadata.archived_at.map(datetime_to_epoch_seconds))
        .bind(metadata.section.as_ref().map(|section| section.id.as_str()))
        .bind(metadata.section_position)
        .bind(metadata.section_entered_at.map(datetime_to_epoch_millis))
        .bind(metadata.git_sha.as_deref())
        .bind(metadata.git_branch.as_deref())
        .bind(metadata.git_origin_url.as_deref())
        .bind("enabled")
        .bind(metadata.project_id.as_deref())
        .bind(metadata.daybreak_enabled)
        .execute(self.pool.as_ref())
        .await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Set the user preference without changing rollout-derived thread metadata.
    pub async fn set_thread_daybreak_enabled(
        &self,
        thread_id: ThreadId,
        daybreak_enabled: bool,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET daybreak_enabled = ? WHERE id = ?")
            .bind(daybreak_enabled)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_thread_memory_mode(
        &self,
        thread_id: ThreadId,
        memory_mode: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET memory_mode = ? WHERE id = ?")
            .bind(memory_mode)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_thread_title(
        &self,
        thread_id: ThreadId,
        title: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET title = ? WHERE id = ?")
            .bind(title)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_thread_name(
        &self,
        thread_id: ThreadId,
        name: Option<&str>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE threads SET name = ? WHERE id = ?")
            .bind(name)
            .bind(thread_id.to_string())
            .execute(self.pool.as_ref())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_updated_at(
        &self,
        thread_id: ThreadId,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let updated_at = self.allocate_thread_updated_at(updated_at)?;
        let result =
            sqlx::query("UPDATE threads SET updated_at = ?, updated_at_ms = ? WHERE id = ?")
                .bind(datetime_to_epoch_seconds(updated_at))
                .bind(datetime_to_epoch_millis(updated_at))
                .bind(thread_id.to_string())
                .execute(self.pool.as_ref())
                .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_thread_recency_at(
        &self,
        thread_id: ThreadId,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<bool> {
        let recency_at = self.allocate_thread_recency_at(recency_at)?;
        let recency_at_seconds = datetime_to_epoch_seconds(recency_at);
        let recency_at_millis = datetime_to_epoch_millis(recency_at);
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    recency_at = MAX(?, MAX(?, recency_at_ms + 1) / 1000),
    recency_at_ms = MAX(?, recency_at_ms + 1)
WHERE id = ?
            "#,
        )
        .bind(recency_at_seconds)
        .bind(recency_at_millis)
        .bind(recency_at_millis)
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Allocate a persisted `updated_at` value for thread-list cursor ordering.
    ///
    /// We keep a process-local high-water mark so hot rollout writes can get unique,
    /// monotonic millisecond timestamps without querying SQLite on every update. Older
    /// backfill/repair timestamps are allowed through unchanged so historical ordering
    /// remains tied to the rollout file mtimes.
    fn allocate_thread_updated_at(
        &self,
        updated_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_updated_at_millis.as_ref(), updated_at)
    }

    fn allocate_thread_recency_at(
        &self,
        recency_at: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        allocate_thread_timestamp(self.thread_recency_at_millis.as_ref(), recency_at)
    }
}

fn allocate_thread_timestamp(
    high_water_mark: &AtomicI64,
    timestamp: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    let candidate = datetime_to_epoch_millis(timestamp);
    let allocated = loop {
        let current = high_water_mark.load(Ordering::Relaxed);

        // New wall-clock time: advance the process-local high-water mark and use it as-is.
        if candidate > current {
            if high_water_mark
                .compare_exchange(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break candidate;
            }
            continue;
        }

        // Older timestamps come from backfill/repair paths that preserve rollout mtimes.
        // Do not drag historical rows forward just because this process has seen newer writes.
        if candidate.saturating_add(1000) <= current {
            break candidate;
        }

        // Same hot one-second bucket as the current high-water mark. Allocate the next
        // millisecond so the timestamp remains unique and cursor-orderable inside the process.
        let bumped = current.saturating_add(1);
        if high_water_mark
            .compare_exchange(current, bumped, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break bumped;
        }
    };
    epoch_millis_to_datetime(allocated)
}

impl StateRuntime {
    pub async fn update_thread_git_info(
        &self,
        thread_id: ThreadId,
        git_sha: Option<Option<&str>>,
        git_branch: Option<Option<&str>>,
        git_origin_url: Option<Option<&SanitizedGitUrl>>,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            r#"
UPDATE threads
SET
    git_sha = CASE WHEN ? THEN ? ELSE git_sha END,
    git_branch = CASE WHEN ? THEN ? ELSE git_branch END,
    git_origin_url = CASE WHEN ? THEN ? ELSE git_origin_url END
WHERE id = ?
            "#,
        )
        .bind(git_sha.is_some())
        .bind(git_sha.flatten())
        .bind(git_branch.is_some())
        .bind(git_branch.flatten())
        .bind(git_origin_url.is_some())
        .bind(git_origin_url.flatten().map(SanitizedGitUrl::as_str))
        .bind(thread_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn upsert_thread_with_creation_memory_mode(
        &self,
        metadata: &crate::ThreadMetadata,
        creation_memory_mode: Option<&str>,
    ) -> anyhow::Result<()> {
        let updated_at = self.allocate_thread_updated_at(metadata.updated_at)?;
        let insert_recency_at = self.allocate_thread_recency_at(metadata.recency_at)?;
        let preview = metadata_preview(metadata);
        // Backfill/reconcile callers merge existing git info before upserting, but that
        // read/modify/write is not atomic. Preserve non-null SQLite git fields here so
        // an explicit metadata update cannot be lost if a stale rollout upsert lands later.
        // Daybreak and project choices are insert-only here; explicit changes use their setters.
        sqlx::query(
            r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    recency_at,
    created_at_ms,
    updated_at_ms,
    recency_at_ms,
    source,
    originator,
    creator_user_id,
    creator_account_id,
    history_mode,
    thread_source,
    agent_nickname,
    agent_role,
    agent_path,
    model_provider,
    model,
    reasoning_effort,
    cwd,
    cli_version,
    title,
    name,
    preview,
    sandbox_policy,
    approval_mode,
    tokens_used,
    first_user_message,
    archived,
    archived_at,
    thread_section_id,
    section_position,
    section_entered_at_ms,
    git_sha,
    git_branch,
    git_origin_url,
    memory_mode,
    project_id,
    daybreak_enabled
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
    rollout_path = excluded.rollout_path,
    created_at = excluded.created_at,
    updated_at = excluded.updated_at,
    recency_at = threads.recency_at,
    created_at_ms = excluded.created_at_ms,
    updated_at_ms = excluded.updated_at_ms,
    recency_at_ms = threads.recency_at_ms,
    source = excluded.source,
    originator = COALESCE(threads.originator, excluded.originator),
    creator_user_id = COALESCE(threads.creator_user_id, excluded.creator_user_id),
    creator_account_id = COALESCE(threads.creator_account_id, excluded.creator_account_id),
    -- Paginated history is a one-way promotion; stale legacy metadata must not downgrade it.
    history_mode = CASE
        WHEN threads.history_mode = 'paginated' THEN threads.history_mode
        ELSE excluded.history_mode
    END,
    thread_source = excluded.thread_source,
    agent_nickname = excluded.agent_nickname,
    agent_role = excluded.agent_role,
    agent_path = excluded.agent_path,
    model_provider = excluded.model_provider,
    model = excluded.model,
    reasoning_effort = excluded.reasoning_effort,
    cwd = excluded.cwd,
    cli_version = excluded.cli_version,
    title = excluded.title,
    preview = COALESCE(NULLIF(excluded.preview, ''), threads.preview),
    tokens_used = excluded.tokens_used,
    first_user_message = excluded.first_user_message,
    archived = excluded.archived,
    archived_at = excluded.archived_at,
    git_sha = COALESCE(threads.git_sha, excluded.git_sha),
    git_branch = COALESCE(threads.git_branch, excluded.git_branch),
    git_origin_url = COALESCE(threads.git_origin_url, excluded.git_origin_url)
            "#,
        )
        .bind(metadata.id.to_string())
        .bind(metadata.rollout_path.display().to_string())
        .bind(datetime_to_epoch_seconds(metadata.created_at))
        .bind(datetime_to_epoch_seconds(updated_at))
        .bind(datetime_to_epoch_seconds(insert_recency_at))
        .bind(datetime_to_epoch_millis(metadata.created_at))
        .bind(datetime_to_epoch_millis(updated_at))
        .bind(datetime_to_epoch_millis(insert_recency_at))
        .bind(metadata.source.as_str())
        .bind(metadata.originator.as_deref())
        .bind(metadata.creator_user_id.as_deref())
        .bind(metadata.creator_account_id.as_deref())
        .bind(metadata.history_mode.as_str())
        .bind(
            metadata
                .thread_source
                .as_ref()
                .map(codex_protocol::protocol::ThreadSource::as_str),
        )
        .bind(metadata.agent_nickname.as_deref())
        .bind(metadata.agent_role.as_deref())
        .bind(metadata.agent_path.as_deref())
        .bind(metadata.model_provider.as_str())
        .bind(metadata.model.as_deref())
        .bind(
            metadata
                .reasoning_effort
                .as_ref()
                .map(crate::extract::enum_to_string),
        )
        .bind(metadata.cwd.display().to_string())
        .bind(metadata.cli_version.as_str())
        .bind(metadata.title.as_str())
        .bind(metadata.name.as_deref())
        .bind(preview)
        .bind(SANDBOX_POLICY_COLUMN_VALUE)
        .bind(APPROVAL_MODE_COLUMN_VALUE)
        .bind(metadata.tokens_used)
        .bind(metadata.first_user_message.as_deref().unwrap_or_default())
        .bind(metadata.archived_at.is_some())
        .bind(metadata.archived_at.map(datetime_to_epoch_seconds))
        .bind(metadata.section.as_ref().map(|section| section.id.as_str()))
        .bind(metadata.section_position)
        .bind(metadata.section_entered_at.map(datetime_to_epoch_millis))
        .bind(metadata.git_sha.as_deref())
        .bind(metadata.git_branch.as_deref())
        .bind(metadata.git_origin_url.as_deref())
        .bind(creation_memory_mode.unwrap_or("enabled"))
        .bind(metadata.project_id.as_deref())
        .bind(metadata.daybreak_enabled)
        .execute(self.pool.as_ref())
        .await?;
        self.insert_thread_spawn_edge_from_source_if_absent(metadata.id, metadata.source.as_str())
            .await?;
        Ok(())
    }

    /// Apply rollout items incrementally using the underlying database.
    pub async fn apply_rollout_items(
        &self,
        builder: &ThreadMetadataBuilder,
        items: &[RolloutItem],
        new_thread_memory_mode: Option<&str>,
        updated_at_override: Option<DateTime<Utc>>,
    ) -> anyhow::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let existing_metadata = self.get_thread(builder.id).await?;
        let mut metadata = existing_metadata
            .clone()
            .unwrap_or_else(|| builder.build(&self.default_provider));
        metadata.rollout_path = builder.rollout_path.clone();
        for item in items {
            apply_rollout_item(&mut metadata, item, &self.default_provider);
        }
        if let Some(existing_metadata) = existing_metadata.as_ref() {
            metadata.prefer_existing_git_info(existing_metadata);
        }
        let updated_at = match updated_at_override {
            Some(updated_at) => Some(updated_at),
            None => file_modified_time_utc(builder.rollout_path.as_path()).await,
        };
        if let Some(updated_at) = updated_at {
            metadata.updated_at = updated_at;
        }
        let upsert_result = if existing_metadata.is_none() {
            self.upsert_thread_with_creation_memory_mode(&metadata, new_thread_memory_mode)
                .await
        } else {
            self.upsert_thread(&metadata).await
        };
        upsert_result?;
        if let Some(memory_mode) = extract_memory_mode(items)
            && let Err(err) = self
                .set_thread_memory_mode(builder.id, memory_mode.as_str())
                .await
        {
            return Err(err);
        }
        Ok(())
    }

    /// Mark a thread as archived using the underlying database.
    pub async fn mark_archived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
        archived_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let Some(mut metadata) = self.get_thread(thread_id).await? else {
            return Ok(());
        };
        metadata.archived_at = Some(archived_at);
        metadata.rollout_path = rollout_path.to_path_buf();
        if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
            metadata.updated_at = updated_at;
        }
        if metadata.id != thread_id {
            warn!(
                "thread id mismatch during archive: expected {thread_id}, got {}",
                metadata.id
            );
        }
        self.upsert_thread(&metadata).await
    }

    /// Mark a thread as unarchived using the underlying database.
    pub async fn mark_unarchived(
        &self,
        thread_id: ThreadId,
        rollout_path: &Path,
    ) -> anyhow::Result<()> {
        let Some(mut metadata) = self.get_thread(thread_id).await? else {
            return Ok(());
        };
        metadata.archived_at = None;
        metadata.rollout_path = rollout_path.to_path_buf();
        if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
            metadata.updated_at = updated_at;
        }
        if metadata.id != thread_id {
            warn!(
                "thread id mismatch during unarchive: expected {thread_id}, got {}",
                metadata.id
            );
        }
        self.upsert_thread(&metadata).await
    }

    /// Delete a thread and all associated state by id.
    pub async fn delete_thread(&self, thread_id: ThreadId) -> anyhow::Result<u64> {
        self.delete_threads_strict(&[thread_id]).await
    }

    /// Delete a set of threads and all associated state.
    ///
    /// Spawn edges and thread rows are deleted last so a failed delete can be retried with enough
    /// state left to rediscover the same spawned subtree.
    pub async fn delete_threads_strict(&self, thread_ids: &[ThreadId]) -> anyhow::Result<u64> {
        if thread_ids.is_empty() {
            return Ok(0);
        }

        let thread_id_strings = thread_ids
            .iter()
            .map(ThreadId::to_string)
            .collect::<Vec<_>>();
        for (thread_id, thread_id_string) in thread_ids.iter().zip(&thread_id_strings) {
            sqlx::query("DELETE FROM logs WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(self.logs_pool.as_ref())
                .await?;
            self.thread_queue.delete_thread_queue(*thread_id).await?;
            self.delete_versioned_thread_memory(*thread_id).await?;
            self.thread_goals.delete_thread_goal(*thread_id).await?;
        }

        let mut tx = self.pool.begin().await?;
        for thread_id_string in &thread_id_strings {
            sqlx::query("DELETE FROM thread_dynamic_tools WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?;
        }
        for thread_id_string in &thread_id_strings {
            sqlx::query(
                "DELETE FROM thread_spawn_edges WHERE parent_thread_id = ? OR child_thread_id = ?",
            )
            .bind(thread_id_string)
            .bind(thread_id_string)
            .execute(&mut *tx)
            .await?;
        }
        let mut rows_affected = 0;
        for thread_id_string in &thread_id_strings {
            sqlx::query("DELETE FROM thread_read_receipts WHERE thread_id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?;
            rows_affected += sqlx::query("DELETE FROM threads WHERE id = ?")
                .bind(thread_id_string)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;

        Ok(rows_affected)
    }
}

fn one_thread_id_from_rows(
    rows: Vec<sqlx::sqlite::SqliteRow>,
    agent_path: &str,
) -> anyhow::Result<Option<ThreadId>> {
    let mut ids = rows
        .into_iter()
        .map(|row| {
            let id: String = row.try_get("id")?;
            ThreadId::try_from(id).map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    match ids.len() {
        0 => Ok(None),
        1 => Ok(ids.pop()),
        _ => Err(anyhow::anyhow!(
            "multiple agents found for canonical path `{agent_path}`"
        )),
    }
}

fn push_list_threads_query(
    builder: &mut QueryBuilder<Sqlite>,
    filters: ThreadFilterOptions<'_>,
    relation_filter: Option<crate::ThreadRelationFilter>,
    limit: usize,
) {
    if let Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) = relation_filter {
        builder.push(
            r#"
WITH RECURSIVE subtree(child_thread_id, parent_thread_id) AS (
    SELECT child_thread_id, parent_thread_id
    FROM thread_spawn_edges
    WHERE parent_thread_id =
"#,
        );
        builder.push_bind(ancestor_thread_id.to_string());
        builder.push(
            r#"
    UNION
    SELECT edge.child_thread_id, edge.parent_thread_id
    FROM thread_spawn_edges AS edge
    JOIN subtree ON edge.parent_thread_id = subtree.child_thread_id
)
"#,
        );
    }
    push_thread_select_columns(builder);
    // SQLite may otherwise reorder these joins and scan the global timestamp index before
    // checking the relationship. CROSS JOIN keeps the selective edge/subtree traversal first.
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(_)) => builder.push(
            ", listed_edge.parent_thread_id AS parent_thread_id\nFROM thread_spawn_edges AS listed_edge\nCROSS JOIN threads ON threads.id = listed_edge.child_thread_id",
        ),
        Some(crate::ThreadRelationFilter::DescendantsOf(_)) => builder.push(
            ", subtree.parent_thread_id AS parent_thread_id\nFROM subtree\nCROSS JOIN threads ON threads.id = subtree.child_thread_id",
        ),
        None => builder.push(" FROM threads"),
    };
    let include_thread_id_tiebreaker = relation_filter.is_some()
        || matches!(
            filters.sort_key,
            SortKey::CreatedAt | SortKey::RecencyAt | SortKey::SectionPosition
        );
    push_thread_filters_with_preview(
        builder,
        filters,
        include_thread_id_tiebreaker,
        /*include_empty_preview*/ relation_filter.is_some(),
    );
    match relation_filter {
        Some(crate::ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
            builder.push(" AND listed_edge.parent_thread_id = ");
            builder.push_bind(parent_thread_id.to_string());
        }
        Some(crate::ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
            builder.push(" AND subtree.child_thread_id != ");
            builder.push_bind(ancestor_thread_id.to_string());
        }
        None => {}
    }
    let order_by_index = match (relation_filter, filters.cwd_filters) {
        // Relationship listings are expected to be much smaller than the global thread table.
        // Prefer the spawn-edge index and sort the matching subtree instead of scanning the
        // timestamp index until enough related threads happen to be found.
        (Some(_), _) => OrderByIndex::Disabled,
        // Multi-cwd listing is supported but at the time of writing has no current use in production.
        // Preserve its query plan so the global timestamp index does not regress cwd filtering into a scan.
        (None, Some(cwd_filters)) if cwd_filters.len() > 1 => OrderByIndex::Disabled,
        (None, Some(_) | None) => OrderByIndex::Enabled,
    };
    push_thread_order_and_limit(
        builder,
        filters.sort_key,
        filters.sort_direction,
        order_by_index,
        include_thread_id_tiebreaker,
        limit,
    );
}

pub(super) fn push_thread_select_columns(builder: &mut QueryBuilder<Sqlite>) {
    builder.push(
        r#"
SELECT
    threads.id,
    threads.rollout_path,
    threads.created_at_ms AS created_at,
    threads.updated_at_ms AS updated_at,
    threads.recency_at_ms AS recency_at,
    threads.source,
    threads.originator,
    threads.creator_user_id,
    threads.creator_account_id,
    threads.history_mode,
    threads.thread_source,
    threads.agent_nickname,
    threads.agent_role,
    threads.agent_path,
    threads.model_provider,
    threads.model,
    threads.reasoning_effort,
    threads.cwd,
    threads.cli_version,
    threads.title,
    threads.name,
    threads.preview,
    threads.tokens_used,
    threads.first_user_message,
    threads.archived_at,
    threads.thread_section_id AS section,
    (
        SELECT thread_sections.name
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_name,
    (
        SELECT thread_sections.appearance
        FROM thread_sections
        WHERE thread_sections.id = threads.thread_section_id
    ) AS section_appearance,
    threads.section_position,
    threads.section_entered_at_ms,
    threads.project_id,
    threads.daybreak_enabled,
    threads.git_sha,
    threads.git_branch,
    threads.git_origin_url
"#,
    );
}

pub(super) fn extract_memory_mode(items: &[RolloutItem]) -> Option<String> {
    items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => meta_line.meta.memory_mode.clone(),
        RolloutItem::ResponseItem(_)
        | RolloutItem::InterAgentCommunication(_)
        | RolloutItem::InterAgentCommunicationMetadata { .. }
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::WorldState(_)
        | RolloutItem::RetainedContext(_)
        | RolloutItem::TokenUsageRecord(_)
        | RolloutItem::EventMsg(_) => None,
    })
}

fn thread_spawn_parent_thread_id_from_source_str(source: &str) -> Option<ThreadId> {
    let parsed_source = serde_json::from_str(source)
        .or_else(|_| serde_json::from_value::<SessionSource>(Value::String(source.to_string())));
    parsed_source.ok()?.parent_thread_id()
}

#[derive(Clone, Copy)]
pub struct ThreadFilterOptions<'a> {
    pub archived_only: bool,
    pub allowed_sources: &'a [String],
    pub model_providers: Option<&'a [String]>,
    pub cwd_filters: Option<&'a [PathBuf]>,
    pub section: Option<Option<&'a str>>,
    pub project_id: Option<Option<&'a str>>,
    pub anchor: Option<&'a crate::Anchor>,
    pub sort_key: SortKey,
    pub sort_direction: SortDirection,
    pub search_term: Option<&'a str>,
}

pub(super) fn push_thread_filters<'a>(
    builder: &mut QueryBuilder<Sqlite>,
    options: ThreadFilterOptions<'a>,
    include_thread_id_tiebreaker: bool,
) {
    push_thread_filters_with_preview(
        builder,
        options,
        include_thread_id_tiebreaker,
        /*include_empty_preview*/ false,
    );
}

fn push_thread_filters_with_preview<'a>(
    builder: &mut QueryBuilder<Sqlite>,
    options: ThreadFilterOptions<'a>,
    include_thread_id_tiebreaker: bool,
    include_empty_preview: bool,
) {
    let ThreadFilterOptions {
        archived_only,
        allowed_sources,
        model_providers,
        cwd_filters,
        section,
        project_id,
        anchor,
        sort_key,
        sort_direction,
        search_term,
    } = options;
    builder.push(" WHERE 1 = 1");
    if archived_only {
        builder.push(" AND threads.archived = 1");
    } else {
        builder.push(" AND threads.archived = 0");
    }
    if !archived_only && !include_empty_preview && !matches!(section, Some(Some(_))) {
        builder.push(" AND threads.preview <> ''");
    }
    match section {
        Some(Some(section)) => {
            builder.push(" AND threads.thread_section_id = ");
            builder.push_bind(section);
        }
        Some(None) => {
            builder.push(" AND threads.thread_section_id IS NULL");
        }
        None => {}
    }
    match project_id {
        Some(Some(project_id)) => {
            builder.push(" AND threads.project_id = ");
            builder.push_bind(project_id);
        }
        Some(None) => {
            builder.push(" AND threads.project_id IS NULL");
        }
        None => {}
    }
    if !allowed_sources.is_empty() {
        builder.push(" AND threads.source IN (");
        let mut separated = builder.separated(", ");
        for source in allowed_sources {
            separated.push_bind(source);
        }
        separated.push_unseparated(")");
    }
    if let Some(model_providers) = model_providers
        && !model_providers.is_empty()
    {
        builder.push(" AND threads.model_provider IN (");
        let mut separated = builder.separated(", ");
        for provider in model_providers {
            separated.push_bind(provider);
        }
        separated.push_unseparated(")");
    }
    match cwd_filters {
        Some([]) => {
            builder.push(" AND 1 = 0");
        }
        Some(cwd_filters) => {
            builder.push(" AND threads.cwd IN (");
            let mut separated = builder.separated(", ");
            for cwd in cwd_filters {
                separated.push_bind(cwd.display().to_string());
            }
            separated.push_unseparated(")");
        }
        None => {}
    }
    if let Some(search_term) = search_term {
        builder.push(" AND (instr(COALESCE(threads.name, ''), ");
        builder.push_bind(search_term);
        builder.push(") > 0 OR instr(threads.title, ");
        builder.push_bind(search_term);
        builder.push(") > 0 OR instr(threads.preview, ");
        builder.push_bind(search_term);
        builder.push(") > 0)");
    }
    if let Some(anchor) = anchor {
        let anchor_ts = datetime_to_epoch_millis(anchor.ts);
        let column = match sort_key {
            SortKey::CreatedAt => "threads.created_at_ms",
            SortKey::UpdatedAt => "threads.updated_at_ms",
            SortKey::RecencyAt => "threads.recency_at_ms",
            SortKey::SectionPosition => "threads.section_position",
        };
        let operator = match sort_direction {
            SortDirection::Asc => ">",
            SortDirection::Desc => "<",
        };
        if include_thread_id_tiebreaker && anchor.id.is_some() {
            // Keep the timestamp index range despite the UUID tie-breaker's OR predicate.
            builder.push(format!(" AND {column} {operator}= "));
            builder.push_bind(anchor_ts);
        }
        builder.push(" AND (");
        builder.push(column);
        builder.push(" ");
        builder.push(operator);
        builder.push(" ");
        builder.push_bind(anchor_ts);
        if include_thread_id_tiebreaker && let Some(anchor_id) = anchor.id {
            builder.push(" OR (");
            builder.push(column);
            builder.push(" = ");
            builder.push_bind(anchor_ts);
            builder.push(" AND threads.id ");
            builder.push(operator);
            builder.push(" ");
            builder.push_bind(anchor_id.to_string());
            builder.push(")");
        }
        builder.push(")");
    }
}

/// Controls whether SQLite may use the ordered column to satisfy `ORDER BY` from an index.
///
/// Disabling it adds a unary `+` to the ordered column. This preserves the sort semantics while
/// preventing a timestamp-only index from winning over a more selective filtering index.
#[derive(Clone, Copy)]
pub(super) enum OrderByIndex {
    Enabled,
    Disabled,
}

pub(super) fn push_thread_order_and_limit(
    builder: &mut QueryBuilder<Sqlite>,
    sort_key: SortKey,
    sort_direction: SortDirection,
    order_by_index: OrderByIndex,
    include_thread_id_tiebreaker: bool,
    limit: usize,
) {
    let order_column = match sort_key {
        SortKey::CreatedAt => "threads.created_at_ms",
        SortKey::UpdatedAt => "threads.updated_at_ms",
        SortKey::RecencyAt => "threads.recency_at_ms",
        SortKey::SectionPosition => "threads.section_position",
    };
    let order_direction = match sort_direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    builder.push(" ORDER BY ");
    match order_by_index {
        OrderByIndex::Enabled => {}
        OrderByIndex::Disabled => {
            builder.push("+");
        }
    }
    builder.push(order_column);
    builder.push(" ");
    builder.push(order_direction);
    if include_thread_id_tiebreaker {
        builder.push(", threads.id ");
        builder.push(order_direction);
    }
    builder.push(" LIMIT ");
    builder.push_bind(limit as i64);
}

fn metadata_preview(metadata: &crate::ThreadMetadata) -> &str {
    metadata
        .preview
        .as_deref()
        .or(metadata.first_user_message.as_deref())
        .unwrap_or_default()
}

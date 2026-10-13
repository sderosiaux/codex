use super::*;

impl StateRuntime {
    pub async fn get_backfill_state(&self) -> anyhow::Result<crate::BackfillState> {
        self.ensure_backfill_state_row().await?;
        let row = sqlx::query(
            r#"
SELECT status, last_watermark, last_success_at
FROM backfill_state
WHERE id = 1
            "#,
        )
        .fetch_one(self.pool.as_ref())
        .await?;
        crate::BackfillState::try_from_row(&row)
    }

    /// Attempt to claim ownership of rollout metadata backfill.
    ///
    /// Returns `true` when this runtime claimed the backfill worker slot.
    /// Returns `false` if backfill is already complete or currently owned by a
    /// non-expired worker.
    pub async fn try_claim_backfill(&self, lease_seconds: i64) -> anyhow::Result<bool> {
        self.ensure_backfill_state_row().await?;
        let now = Utc::now().timestamp();
        let lease_cutoff = now.saturating_sub(lease_seconds.max(0));
        let result = sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, updated_at = ?
WHERE id = 1
  AND status != ?
  AND (status != ? OR updated_at <= ?)
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(now)
        .bind(crate::BackfillStatus::Complete.as_str())
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(lease_cutoff)
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Mark rollout metadata backfill as running.
    pub async fn mark_backfill_running(&self) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, updated_at = ?
WHERE id = 1
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Persist rollout metadata backfill progress.
    pub async fn checkpoint_backfill(&self, watermark: &str) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        sqlx::query(
            r#"
UPDATE backfill_state
SET status = ?, last_watermark = ?, updated_at = ?
WHERE id = 1
            "#,
        )
        .bind(crate::BackfillStatus::Running.as_str())
        .bind(watermark)
        .bind(Utc::now().timestamp())
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Mark rollout metadata backfill as complete.
    pub async fn mark_backfill_complete(&self, last_watermark: Option<&str>) -> anyhow::Result<()> {
        self.ensure_backfill_state_row().await?;
        let now = Utc::now().timestamp();
        sqlx::query(
            r#"
UPDATE backfill_state
SET
    status = ?,
    last_watermark = COALESCE(?, last_watermark),
    last_success_at = ?,
    updated_at = ?
WHERE id = 1
            "#,
        )
        .bind(crate::BackfillStatus::Complete.as_str())
        .bind(last_watermark)
        .bind(now)
        .bind(now)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    async fn ensure_backfill_state_row(&self) -> anyhow::Result<()> {
        ensure_backfill_state_row_in_pool(self.pool.as_ref()).await
    }
}

use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Utc;
use codex_git_utils::collect_git_info;
use codex_git_utils::get_git_repo_root;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::GitInfo;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::protocol::user_message_preview;
use codex_rollout::RolloutItem;
use codex_state::ThreadMetadata;

use crate::CreateThreadParams;
use crate::GitInfoPatch;
use crate::ResumeThreadParams;
use crate::ThreadMetadataPatch;
use crate::types::canonical_history_mode_from_rollout_items;

const THREAD_UPDATED_AT_TOUCH_INTERVAL: Duration = Duration::from_secs(5);

/// Live-thread helper that derives metadata updates from appended rollout items.
///
/// Stores receive raw rollout items plus explicit metadata patches. This helper
/// keeps append-derived metadata observation in the live layer without owning persistence-policy
/// filtering or making `append_items` infer metadata inside a `ThreadStore` implementation.
pub(crate) struct ThreadMetadataSync {
    thread_id: ThreadId,
    cwd_seen: bool,
    preview_seen: bool,
    first_user_message_seen: bool,
    title_seen: bool,
    pending_update: Option<ThreadMetadataPatch>,
    pending_update_generation: u64,
    last_touch_persisted_at: Option<Instant>,
    defer_create_update_until_history_exists: bool,
    defer_resume_update_until_append: bool,
}

pub(crate) struct PendingThreadMetadataPatch {
    pub(crate) patch: ThreadMetadataPatch,
    generation: u64,
}

impl ThreadMetadataSync {
    pub(crate) async fn for_create(params: &CreateThreadParams) -> Self {
        let created_at = Utc::now();
        let cwd = params.metadata.cwd.clone().unwrap_or_default();
        let git_info = if get_git_repo_root(cwd.as_path()).is_some() {
            collect_git_info(cwd.as_path()).await.map(|info| GitInfo {
                commit_hash: info.commit_hash,
                branch: info.branch,
                repository_url: info.repository_url,
            })
        } else {
            None
        };
        let update = ThreadMetadataPatch {
            model_provider: Some(params.metadata.model_provider.clone()),
            created_at: Some(created_at),
            updated_at: Some(created_at),
            source: Some(params.source.clone()),
            originator: (!params.originator.is_empty()).then(|| params.originator.clone()),
            creator_user_id: params.creator_user_id.clone(),
            creator_account_id: params.creator_account_id.clone(),
            thread_source: Some(params.thread_source.clone()),
            agent_nickname: Some(params.source.get_nickname()),
            agent_role: Some(params.source.get_agent_role()),
            agent_path: Some(params.source.get_agent_path().map(Into::into)),
            cwd: Some(cwd.clone()),
            cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            git_info: git_info.map(git_info_patch_from_observation),
            memory_mode: Some(params.metadata.memory_mode),
            ..Default::default()
        };
        Self {
            thread_id: params.thread_id,
            cwd_seen: !cwd.as_os_str().is_empty(),
            preview_seen: false,
            first_user_message_seen: false,
            title_seen: false,
            pending_update: Some(update),
            pending_update_generation: 1,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: true,
            defer_resume_update_until_append: false,
        }
    }

    pub(crate) fn for_resume(
        params: &ResumeThreadParams,
        metadata: Option<&ThreadMetadata>,
    ) -> Self {
        let mut sync = Self {
            thread_id: params.thread_id,
            cwd_seen: params
                .metadata
                .cwd
                .as_ref()
                .is_some_and(|cwd| !cwd.as_os_str().is_empty()),
            preview_seen: metadata
                .and_then(|metadata| metadata.preview.as_deref())
                .is_some_and(|preview| !preview.is_empty()),
            first_user_message_seen: metadata
                .is_some_and(|metadata| metadata.first_user_message.is_some()),
            title_seen: metadata.is_some_and(|metadata| !metadata.title.is_empty()),
            pending_update: None,
            pending_update_generation: 0,
            last_touch_persisted_at: None,
            defer_create_update_until_history_exists: false,
            defer_resume_update_until_append: false,
        };
        if let Some(history) = params.history.as_deref() {
            sync.record_resume_history(history);
        }
        sync
    }

    pub(crate) fn record_resume_history(&mut self, history: &[RolloutItem]) {
        let update = self.observe_resume_history(history);
        self.merge_pending_update(update);
        self.defer_resume_update_until_append = self.pending_update.is_some();
    }

    pub(crate) fn take_pending_update(&self) -> Option<PendingThreadMetadataPatch> {
        self.pending_update
            .clone()
            .map(|patch| PendingThreadMetadataPatch {
                patch,
                generation: self.pending_update_generation,
            })
    }

    pub(crate) fn take_pending_update_for_existing_history(
        &self,
    ) -> Option<PendingThreadMetadataPatch> {
        if self.defer_create_update_until_history_exists {
            return None;
        }
        if self.defer_resume_update_until_append {
            return None;
        }
        self.take_pending_update()
    }

    pub(crate) fn mark_pending_update_applied(&mut self, update: &PendingThreadMetadataPatch) {
        if self.pending_update_generation == update.generation {
            self.pending_update = None;
        }
        if update.patch.updated_at.is_some() {
            self.last_touch_persisted_at = Some(Instant::now());
        }
    }

    pub(crate) fn observe_appended_items(
        &mut self,
        items: &[RolloutItem],
    ) -> Option<PendingThreadMetadataPatch> {
        self.defer_create_update_until_history_exists = false;
        self.defer_resume_update_until_append = false;
        let affects_metadata = items
            .iter()
            .any(codex_state::rollout_item_affects_thread_metadata);
        let advances_recency = items
            .iter()
            .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))));
        let mut update = if affects_metadata {
            self.observe_items(items)?
        } else {
            thread_updated_at_touch()
        };
        if advances_recency {
            update.advance_recency_at = Some(Utc::now());
        }
        self.merge_pending_update(Some(update));
        if !affects_metadata
            && !self
                .pending_update
                .as_ref()
                .is_some_and(update_has_metadata_facts)
            && self.last_touch_persisted_at.is_some_and(|last_touch| {
                Instant::now().duration_since(last_touch) < THREAD_UPDATED_AT_TOUCH_INTERVAL
            })
        {
            return None;
        }
        self.take_pending_update()
    }

    fn observe_items(&mut self, items: &[RolloutItem]) -> Option<ThreadMetadataPatch> {
        self.observe_items_with_update(
            items,
            ThreadMetadataPatch {
                updated_at: Some(Utc::now()),
                ..Default::default()
            },
        )
    }

    fn observe_resume_history(&mut self, items: &[RolloutItem]) -> Option<ThreadMetadataPatch> {
        let mut update = self.observe_items_with_update(items, ThreadMetadataPatch::default())?;
        if matches!(
            canonical_history_mode_from_rollout_items(items),
            ThreadHistoryMode::Paginated
        ) {
            // Paginated rollouts never append metadata-only SessionMeta updates. Do not reapply
            // initial metadata when resume history is flushed after the first append.
            update.git_info = None;
            update.memory_mode = None;
        }
        Some(update)
    }

    fn observe_items_with_update(
        &mut self,
        items: &[RolloutItem],
        mut update: ThreadMetadataPatch,
    ) -> Option<ThreadMetadataPatch> {
        if items.is_empty() {
            return None;
        }
        for item in items {
            match item {
                RolloutItem::SessionMeta(meta_line) if meta_line.meta.id == self.thread_id => {
                    update.created_at = parse_session_timestamp(meta_line.meta.timestamp.as_str());
                    update.creator_user_id = meta_line.meta.creator_user_id.clone();
                    update.creator_account_id = meta_line.meta.creator_account_id.clone();
                    update.source = Some(meta_line.meta.source.clone());
                    if !meta_line.meta.originator.is_empty() {
                        update.originator = Some(meta_line.meta.originator.clone());
                    }
                    update.thread_source = Some(meta_line.meta.thread_source.clone());
                    update.agent_nickname = Some(meta_line.meta.agent_nickname.clone());
                    update.agent_role = Some(meta_line.meta.agent_role.clone());
                    update.agent_path = Some(meta_line.meta.agent_path.clone());
                    if let Some(model_provider) = meta_line.meta.model_provider.clone()
                        && !model_provider.is_empty()
                    {
                        update.model_provider = Some(model_provider);
                    }
                    if !meta_line.meta.cli_version.is_empty() {
                        update.cli_version = Some(meta_line.meta.cli_version.clone());
                    }
                    if !meta_line.meta.cwd.as_os_str().is_empty() {
                        self.cwd_seen = true;
                        update.cwd = Some(meta_line.meta.cwd.clone());
                    }
                    if let Some(git_info) = meta_line.git.clone() {
                        update.git_info = Some(git_info_patch_from_observation(git_info));
                    }
                    if let Some(memory_mode) = meta_line.meta.memory_mode.as_deref()
                        && let Some(memory_mode) = parse_memory_mode(memory_mode)
                    {
                        update.memory_mode = Some(memory_mode);
                    }
                }
                RolloutItem::TurnContext(turn_ctx) => {
                    if !self.cwd_seen {
                        self.cwd_seen = true;
                        update.cwd = Some(turn_ctx.cwd.clone().into_path_buf());
                    }
                    update.model = Some(turn_ctx.model.clone());
                    update.reasoning_effort = Some(turn_ctx.effort.clone());
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(user)) => {
                    self.observe_user_message(user, &mut update);
                }
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                    if let TurnItem::UserMessage(user) = &event.item
                        && (!self.first_user_message_seen || !self.preview_seen || !self.title_seen)
                    {
                        self.observe_user_message(
                            &user.as_legacy_user_message_event(),
                            &mut update,
                        );
                    } else if let TurnItem::FunctionCallOutput(output) = &event.item
                        && !self.preview_seen
                        && let Some(preview) = codex_state::delegated_output_preview(output)
                    {
                        self.preview_seen = true;
                        update.preview = Some(preview);
                    }
                }
                RolloutItem::EventMsg(EventMsg::TokenCount(token_count)) => {
                    if let Some(info) = token_count.info.as_ref() {
                        update.token_usage = Some(info.total_token_usage.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadGoalUpdated(event)) => {
                    if !self.preview_seen {
                        let objective = event.goal.objective.trim();
                        if !objective.is_empty() {
                            self.preview_seen = true;
                            update.preview = Some(objective.to_string());
                        }
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                    let settings = &event.thread_settings;
                    self.cwd_seen = true;
                    update.model = Some(settings.model.clone());
                    update.model_provider = Some(settings.model_provider_id.clone());
                    update.reasoning_effort = Some(settings.reasoning_effort.clone());
                    update.cwd = Some(settings.cwd.clone().into_path_buf());
                }
                RolloutItem::SessionMeta(_)
                | RolloutItem::EventMsg(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::Compacted(_)
                | RolloutItem::TokenUsageRecord(_)
                | RolloutItem::RetainedContext(_)
                | RolloutItem::WorldState(_) => {}
            }
        }
        Some(update)
    }

    fn observe_user_message(&mut self, user: &UserMessageEvent, update: &mut ThreadMetadataPatch) {
        if (!self.first_user_message_seen || !self.preview_seen)
            && let Some(preview) = user_message_preview(user)
        {
            if !self.first_user_message_seen {
                self.first_user_message_seen = true;
                update.first_user_message = Some(preview.clone());
            }
            if !self.preview_seen {
                self.preview_seen = true;
                update.preview = Some(preview);
            }
        }
        if !self.title_seen {
            let title = strip_user_message_prefix(user.message.as_str());
            if !title.is_empty() {
                self.title_seen = true;
                update.title = Some(title.to_string());
            }
        }
    }

    fn merge_pending_update(&mut self, update: Option<ThreadMetadataPatch>) {
        let Some(update) = update else {
            return;
        };
        match self.pending_update.as_mut() {
            Some(pending_update) => pending_update.merge(update),
            None => self.pending_update = Some(update),
        }
        self.pending_update_generation = self.pending_update_generation.wrapping_add(1);
    }
}

fn parse_memory_mode(value: &str) -> Option<ThreadMemoryMode> {
    match value {
        "enabled" => Some(ThreadMemoryMode::Enabled),
        "disabled" => Some(ThreadMemoryMode::Disabled),
        _ => None,
    }
}

fn parse_session_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H-%M-%S")
                .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc))
        })
        .ok()
}

fn thread_updated_at_touch() -> ThreadMetadataPatch {
    ThreadMetadataPatch {
        updated_at: Some(Utc::now()),
        ..Default::default()
    }
}

fn update_has_metadata_facts(update: &ThreadMetadataPatch) -> bool {
    update.rollout_path.is_some()
        || update.preview.is_some()
        || update.title.is_some()
        || update.model_provider.is_some()
        || update.model.is_some()
        || update.reasoning_effort.is_some()
        || update.created_at.is_some()
        || update.advance_recency_at.is_some()
        || update.source.is_some()
        || update.originator.is_some()
        || update.creator_user_id.is_some()
        || update.creator_account_id.is_some()
        || update.thread_source.is_some()
        || update.agent_nickname.is_some()
        || update.agent_role.is_some()
        || update.agent_path.is_some()
        || update.cwd.is_some()
        || update.cli_version.is_some()
        || update.token_usage.is_some()
        || update.first_user_message.is_some()
        || update.git_info.is_some()
        || update.memory_mode.is_some()
}

fn git_info_patch_from_observation(git_info: GitInfo) -> GitInfoPatch {
    GitInfoPatch {
        sha: git_info.commit_hash.map(|sha| Some(sha.0)),
        branch: git_info.branch.map(Some),
        origin_url: git_info.repository_url.map(Some),
    }
}

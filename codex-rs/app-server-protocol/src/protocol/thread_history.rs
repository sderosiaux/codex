use crate::protocol::item_builders::build_command_execution_begin_item;
use crate::protocol::item_builders::build_command_execution_end_item;
use crate::protocol::item_builders::build_file_change_begin_item;
use crate::protocol::item_builders::build_file_change_end_item;
use crate::protocol::item_builders::review_output_text;
use crate::protocol::v2::CollabAgentState;
use crate::protocol::v2::CollabAgentTool;
use crate::protocol::v2::CollabAgentToolCallStatus;
use crate::protocol::v2::DynamicToolCallOutputContentItem;
use crate::protocol::v2::DynamicToolCallStatus;
use crate::protocol::v2::ImageReference;
use crate::protocol::v2::McpToolCallAppContext;
use crate::protocol::v2::McpToolCallError;
use crate::protocol::v2::McpToolCallResult;
use crate::protocol::v2::McpToolCallStatus;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::Turn;
use crate::protocol::v2::TurnError as V2TurnError;
use crate::protocol::v2::TurnError;
use crate::protocol::v2::TurnItemsView;
use crate::protocol::v2::TurnStatus;
use crate::protocol::v2::UserInput;
use crate::protocol::v2::WebSearchItem;
use crate::protocol::v2::web_search_action_from_core;
use codex_extension_items::image_generation::ImageGenerationItem;
use codex_protocol::items::parse_hook_prompt_message;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::AgentReasoningRawContentEvent;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ContextCompactedEvent;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::UserMessageImageKind;
use codex_protocol::protocol::ViewImageToolCallEvent;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use std::collections::HashMap;
use tracing::warn;
use uuid::Uuid;

/// Convert persisted [`RolloutItem`] entries into a sequence of [`Turn`] values.
///
/// When available, this uses `TurnContext.turn_id` as the canonical turn id so
/// resumed/rebuilt thread history preserves the original turn identifiers.
pub fn build_turns_from_rollout_items(items: &[RolloutItem]) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    for item in items {
        builder.handle_rollout_item(item);
    }
    builder.finish()
}

/// A materialized `ThreadItem` snapshot that changed while handling one input.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadHistoryItemChange {
    pub turn_id: String,
    pub item: ThreadItem,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
}

/// Turn metadata for history projection and snapshots that do not need items.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadHistoryTurnMetadata {
    pub turn_id: String,
    pub root_turn_id: Option<String>,
    pub status: TurnStatus,
    pub error: Option<TurnError>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// Incremental changes produced by opt-in `ThreadHistoryBuilder` handlers.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ThreadHistoryChangeSet {
    pub changed_items: Vec<ThreadHistoryItemChange>,
    pub changed_turns: Vec<ThreadHistoryTurnMetadata>,
    pub removed_turn_ids: Vec<String>,
}

impl ThreadHistoryChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changed_items.is_empty()
            && self.changed_turns.is_empty()
            && self.removed_turn_ids.is_empty()
    }
}

impl From<ThreadHistoryTurnMetadata> for Turn {
    /// Builds a wire turn whose items have not been loaded.
    fn from(value: ThreadHistoryTurnMetadata) -> Self {
        Self {
            id: value.turn_id,
            root_turn_id: value.root_turn_id,
            items: Vec::new(),
            items_view: TurnItemsView::NotLoaded,
            error: value.error,
            status: value.status,
            started_at: value.started_at,
            completed_at: value.completed_at,
            duration_ms: value.duration_ms,
        }
    }
}

impl ThreadHistoryTurnMetadata {
    fn from_pending_turn(turn: &PendingTurn) -> Self {
        Self {
            turn_id: turn.id.clone(),
            root_turn_id: turn.root_turn_id.clone(),
            status: turn.status.clone(),
            error: turn.error.clone(),
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            duration_ms: turn.duration_ms,
        }
    }
}

/// Coalesces per-rollout-item changes into an end-of-batch view. It preserves
/// first-change order while replacing repeated item/turn snapshots with their
/// latest value, and drops accumulated changes for turns removed by rollback.
#[derive(Default)]
struct ThreadHistoryChangeAccumulator {
    changed_items: Vec<Option<ThreadHistoryItemChange>>,
    changed_item_indexes: HashMap<(String, String), usize>,
    changed_turns: Vec<Option<ThreadHistoryTurnMetadata>>,
    changed_turn_indexes: HashMap<String, usize>,
    removed_turn_ids: Vec<String>,
    removed_turn_indexes: HashMap<String, usize>,
}

impl ThreadHistoryChangeAccumulator {
    fn push(&mut self, changes: ThreadHistoryChangeSet) {
        for turn_id in changes.removed_turn_ids {
            self.push_removed_turn_id(turn_id);
        }
        for item_change in changes.changed_items {
            self.push_item_change(item_change);
        }
        for turn_change in changes.changed_turns {
            self.push_turn_change(turn_change);
        }
    }

    fn finish(self) -> ThreadHistoryChangeSet {
        ThreadHistoryChangeSet {
            changed_items: self.changed_items.into_iter().flatten().collect(),
            changed_turns: self.changed_turns.into_iter().flatten().collect(),
            removed_turn_ids: self.removed_turn_ids,
        }
    }

    fn push_item_change(&mut self, change: ThreadHistoryItemChange) {
        let key = (change.turn_id.clone(), change.item.id().to_string());
        if let Some(index) = self.changed_item_indexes.get(&key).copied() {
            self.changed_items[index] = Some(change);
            return;
        }

        self.changed_item_indexes
            .insert(key, self.changed_items.len());
        self.changed_items.push(Some(change));
    }

    fn push_turn_change(&mut self, change: ThreadHistoryTurnMetadata) {
        if let Some(index) = self.changed_turn_indexes.get(&change.turn_id).copied() {
            self.changed_turns[index] = Some(change);
            return;
        }

        self.changed_turn_indexes
            .insert(change.turn_id.clone(), self.changed_turns.len());
        self.changed_turns.push(Some(change));
    }

    fn push_removed_turn_id(&mut self, turn_id: String) {
        if !self.removed_turn_indexes.contains_key(&turn_id) {
            self.removed_turn_indexes
                .insert(turn_id.clone(), self.removed_turn_ids.len());
            self.removed_turn_ids.push(turn_id.clone());
        }

        if let Some(index) = self.changed_turn_indexes.remove(&turn_id) {
            self.changed_turns[index] = None;
        }

        let removed_item_keys: Vec<(String, String)> = self
            .changed_item_indexes
            .keys()
            .filter(|(item_turn_id, _)| item_turn_id == &turn_id)
            .cloned()
            .collect();
        for key in removed_item_keys {
            if let Some(index) = self.changed_item_indexes.remove(&key) {
                self.changed_items[index] = None;
            }
        }
    }
}

pub struct ThreadHistoryBuilder {
    // Retain the builder representation so late completions can reuse each
    // finished turn's item index without adding it to the public Turn type.
    turns: Vec<PendingTurn>,
    current_turn: Option<PendingTurn>,
    next_item_index: i64,
    current_rollout_index: usize,
    next_rollout_index: usize,
    active_change_set: Option<ThreadHistoryChangeSet>,
}

impl Default for ThreadHistoryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadHistoryBuilder {
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            current_turn: None,
            next_item_index: 1,
            current_rollout_index: 0,
            next_rollout_index: 0,
            active_change_set: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn finish(mut self) -> Vec<Turn> {
        self.finish_current_turn();
        self.turns.into_iter().map(Turn::from).collect()
    }

    pub fn active_turn_snapshot(&self) -> Option<Turn> {
        self.active_turn_snapshot_with_items_view(TurnItemsView::Full)
    }

    /// Snapshots the active or last finished turn, copying only the requested items.
    pub fn active_turn_snapshot_with_items_view(&self, items_view: TurnItemsView) -> Option<Turn> {
        self.current_turn
            .as_ref()
            .or_else(|| self.turns.last())
            .map(|turn| turn.snapshot(items_view))
    }

    /// Snapshots active turn metadata without cloning its items.
    /// Like the full snapshot, falls back to the last finished turn.
    pub fn active_turn_metadata_snapshot(&self) -> Option<ThreadHistoryTurnMetadata> {
        self.current_turn
            .as_ref()
            .or_else(|| self.turns.last())
            .map(ThreadHistoryTurnMetadata::from_pending_turn)
    }

    /// Returns the id of the active turn without materializing its items.
    pub fn active_turn_id(&self) -> Option<&str> {
        self.current_turn
            .as_ref()
            .map(|turn| turn.id.as_str())
            .or_else(|| self.turns.last().map(|turn| turn.id.as_str()))
    }

    pub fn turn_snapshot(&self, turn_id: &str) -> Option<Turn> {
        self.current_turn
            .as_ref()
            .filter(|turn| turn.id == turn_id)
            .map(Turn::from)
            .or_else(|| {
                self.turns
                    .iter()
                    .find(|turn| turn.id == turn_id)
                    .map(Turn::from)
            })
    }

    /// Returns the index of the active turn snapshot within the finished turn list.
    ///
    /// When a turn is still open, this is the index it will occupy after
    /// `finish`. When no turn is open, it is the index of the last finished turn.
    pub fn active_turn_position(&self) -> Option<usize> {
        if self.current_turn.is_some() {
            Some(self.turns.len())
        } else if self.turns.is_empty() {
            None
        } else {
            Some(self.turns.len() - 1)
        }
    }

    pub fn has_active_turn(&self) -> bool {
        self.current_turn.is_some()
    }

    pub fn active_turn_id_if_explicit(&self) -> Option<String> {
        self.current_turn
            .as_ref()
            .filter(|turn| turn.opened_explicitly)
            .map(|turn| turn.id.clone())
    }

    pub fn active_turn_start_index(&self) -> Option<usize> {
        self.current_turn
            .as_ref()
            .map(|turn| turn.rollout_start_index)
    }

    /// Shared reducer for persisted rollout replay and in-memory current-turn
    /// tracking used by running thread resume/rejoin.
    ///
    /// This function should handle all EventMsg variants that can be persisted in a rollout file.
    /// See `persisted_rollout_item` in `codex-rs/rollout/src/policy.rs`.
    pub fn handle_event(&mut self, event: &EventMsg) {
        match event {
            EventMsg::UserMessage(payload) => self.handle_user_message(payload),
            EventMsg::AgentMessage(payload) => self.handle_agent_message(payload),
            EventMsg::AgentReasoning(payload) => self.handle_agent_reasoning(payload),
            EventMsg::AgentReasoningRawContent(payload) => {
                self.handle_agent_reasoning_raw_content(payload)
            }
            EventMsg::WebSearchBegin(payload) => self.handle_web_search_begin(payload),
            EventMsg::WebSearchEnd(payload) => self.handle_web_search_end(payload),
            EventMsg::ExecCommandBegin(payload) => self.handle_exec_command_begin(payload),
            EventMsg::ExecCommandEnd(payload) => self.handle_exec_command_end(payload),
            EventMsg::PatchApplyBegin(payload) => self.handle_patch_apply_begin(payload),
            EventMsg::PatchApplyEnd(payload) => self.handle_patch_apply_end(payload),
            EventMsg::DynamicToolCallRequest(payload) => {
                self.handle_dynamic_tool_call_request(payload)
            }
            EventMsg::DynamicToolCallResponse(payload) => {
                self.handle_dynamic_tool_call_response(payload)
            }
            EventMsg::McpToolCallBegin(payload) => self.handle_mcp_tool_call_begin(payload),
            EventMsg::McpToolCallEnd(payload) => self.handle_mcp_tool_call_end(payload),
            EventMsg::ViewImageToolCall(payload) => self.handle_view_image_tool_call(payload),
            EventMsg::ImageGenerationBegin(payload) => self.handle_image_generation_begin(payload),
            EventMsg::ImageGenerationEnd(payload) => self.handle_image_generation_end(payload),
            EventMsg::CollabAgentSpawnBegin(payload) => {
                self.handle_collab_agent_spawn_begin(payload)
            }
            EventMsg::CollabAgentSpawnEnd(payload) => self.handle_collab_agent_spawn_end(payload),
            EventMsg::CollabAgentInteractionBegin(payload) => {
                self.handle_collab_agent_interaction_begin(payload)
            }
            EventMsg::CollabAgentInteractionEnd(payload) => {
                self.handle_collab_agent_interaction_end(payload)
            }
            EventMsg::SubAgentActivity(payload) => self.handle_sub_agent_activity(payload),
            EventMsg::CollabWaitingBegin(payload) => self.handle_collab_waiting_begin(payload),
            EventMsg::CollabWaitingEnd(payload) => self.handle_collab_waiting_end(payload),
            EventMsg::CollabCloseBegin(payload) => self.handle_collab_close_begin(payload),
            EventMsg::CollabCloseEnd(payload) => self.handle_collab_close_end(payload),
            EventMsg::CollabResumeBegin(payload) => self.handle_collab_resume_begin(payload),
            EventMsg::CollabResumeEnd(payload) => self.handle_collab_resume_end(payload),
            EventMsg::ContextCompacted(payload) => self.handle_context_compacted(payload),
            EventMsg::EnteredReviewMode(payload) => self.handle_entered_review_mode(payload),
            EventMsg::ExitedReviewMode(payload) => self.handle_exited_review_mode(payload),
            EventMsg::ItemStarted(payload) => self.handle_item_started(payload),
            EventMsg::ItemCompleted(payload) => self.handle_item_completed(payload),
            EventMsg::HookStarted(_) | EventMsg::HookCompleted(_) => {}
            EventMsg::Error(payload) => self.handle_error(payload),
            EventMsg::TokenCount(_) => {}
            EventMsg::ThreadRolledBack(payload) => self.handle_thread_rollback(payload),
            EventMsg::TurnAborted(payload) => self.handle_turn_aborted(payload),
            EventMsg::TurnStarted(payload) => self.handle_turn_started(payload),
            EventMsg::TurnComplete(payload) => self.handle_turn_complete(payload),
            _ => {}
        }
    }

    pub fn handle_rollout_item(&mut self, item: &RolloutItem) {
        self.current_rollout_index = self.next_rollout_index;
        self.next_rollout_index += 1;
        match item {
            RolloutItem::EventMsg(event) => self.handle_event(event),
            RolloutItem::Compacted(payload) => self.handle_compacted(payload),
            RolloutItem::ResponseItem(item) => self.handle_response_item(&item.item),
            RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::TurnContext(_)
            | RolloutItem::TokenUsageRecord(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::RetainedContext(_)
            | RolloutItem::SessionMeta(_) => {}
        }
    }

    /// Handles one event and returns the materialized items or turn metadata
    /// changed by that event.
    pub fn handle_event_with_changes(&mut self, event: &EventMsg) -> ThreadHistoryChangeSet {
        self.collect_changes(|builder| builder.handle_event(event))
    }

    /// Handles a rollout item and returns the materialized items or turn metadata
    /// changed by that one append.
    pub fn handle_rollout_item_with_changes(
        &mut self,
        item: &RolloutItem,
    ) -> ThreadHistoryChangeSet {
        self.collect_changes(|builder| builder.handle_rollout_item(item))
    }

    /// Handles rollout items in order and returns a coalesced end-of-batch
    /// change set. Multiple changes to the same item or turn are deduplicated
    /// so only the latest snapshot is emitted.
    pub fn handle_rollout_items_with_changes(
        &mut self,
        items: &[RolloutItem],
    ) -> ThreadHistoryChangeSet {
        let mut accumulator = ThreadHistoryChangeAccumulator::default();
        for item in items {
            accumulator.push(self.handle_rollout_item_with_changes(item));
        }
        accumulator.finish()
    }

    fn collect_changes(&mut self, handle: impl FnOnce(&mut Self)) -> ThreadHistoryChangeSet {
        debug_assert!(self.active_change_set.is_none());
        self.active_change_set = Some(ThreadHistoryChangeSet::default());
        handle(self);
        self.active_change_set.take().unwrap_or_default()
    }

    fn handle_response_item(&mut self, item: &codex_protocol::models::ResponseItem) {
        let codex_protocol::models::ResponseItem::Message {
            role, content, id, ..
        } = item
        else {
            return;
        };

        if role != "user" {
            return;
        }

        let Some(hook_prompt) = parse_hook_prompt_message(id.as_deref(), content) else {
            return;
        };

        self.push_item_in_current_turn(ThreadItem::HookPrompt {
            id: hook_prompt.id,
            fragments: hook_prompt
                .fragments
                .into_iter()
                .map(crate::protocol::v2::HookPromptFragment::from)
                .collect(),
        });
    }

    fn handle_user_message(&mut self, payload: &UserMessageEvent) {
        // User messages should stay in explicitly opened turns. For backward
        // compatibility with older streams that did not open turns explicitly,
        // close any implicit/inactive turn and start a fresh one for this input.
        if let Some(turn) = self.current_turn.as_ref()
            && !turn.opened_explicitly
            && !(turn.saw_compaction && turn.items.is_empty())
        {
            self.finish_current_turn();
        }
        let id = self.next_item_id();
        let content = self.build_user_inputs(payload);
        self.push_item_in_current_turn(ThreadItem::UserMessage {
            id,
            client_id: payload.client_id.clone(),
            content,
        });
    }

    fn handle_agent_message(&mut self, payload: &AgentMessageEvent) {
        if payload.message.is_empty() {
            return;
        }

        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::AgentMessage {
            id,
            text: payload.message.clone(),
            phase: payload.phase.clone(),
            memory_citation: payload.memory_citation.clone().map(Into::into),
            delivery: payload.delivery,
            questions: payload.questions.clone(),
        });
    }

    fn handle_agent_reasoning(&mut self, payload: &AgentReasoningEvent) {
        if payload.text.is_empty() {
            return;
        }

        // If the last item is a reasoning item, add the new text to the summary.
        let existing_item_change = {
            let tracking_changes = self.is_tracking_changes();
            let turn = self.ensure_turn();
            if let Some(ThreadItem::Reasoning { summary, .. }) = turn.items.last_mut() {
                summary.push(payload.text.clone());
                let changed_item = if tracking_changes {
                    turn.items
                        .last()
                        .cloned()
                        .map(|item| (turn.id.clone(), item))
                } else {
                    None
                };
                Some(changed_item)
            } else {
                None
            }
        };
        if let Some(changed_item) = existing_item_change {
            if let Some((turn_id, item)) = changed_item {
                self.record_changed_item(turn_id, item);
            }
            return;
        }

        // Otherwise, create a new reasoning item.
        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::Reasoning {
            id,
            summary: vec![payload.text.clone()],
            content: Vec::new(),
        });
    }

    fn handle_agent_reasoning_raw_content(&mut self, payload: &AgentReasoningRawContentEvent) {
        if payload.text.is_empty() {
            return;
        }

        // If the last item is a reasoning item, add the new text to the content.
        let existing_item_change = {
            let tracking_changes = self.is_tracking_changes();
            let turn = self.ensure_turn();
            if let Some(ThreadItem::Reasoning { content, .. }) = turn.items.last_mut() {
                content.push(payload.text.clone());
                let changed_item = if tracking_changes {
                    turn.items
                        .last()
                        .cloned()
                        .map(|item| (turn.id.clone(), item))
                } else {
                    None
                };
                Some(changed_item)
            } else {
                None
            }
        };
        if let Some(changed_item) = existing_item_change {
            if let Some((turn_id, item)) = changed_item {
                self.record_changed_item(turn_id, item);
            }
            return;
        }

        // Otherwise, create a new reasoning item.
        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::Reasoning {
            id,
            summary: Vec::new(),
            content: vec![payload.text.clone()],
        });
    }

    fn handle_item_started(&mut self, payload: &ItemStartedEvent) {
        self.handle_materialized_item_lifecycle(&payload.turn_id, &payload.item);
    }

    fn handle_item_completed(&mut self, payload: &ItemCompletedEvent) {
        self.handle_materialized_item_lifecycle(&payload.turn_id, &payload.item);
    }

    fn handle_materialized_item_lifecycle(
        &mut self,
        turn_id: &str,
        item: &codex_protocol::items::TurnItem,
    ) {
        let is_review_mode_item = matches!(
            item,
            codex_protocol::items::TurnItem::EnteredReviewMode(_)
                | codex_protocol::items::TurnItem::ExitedReviewMode(_)
        );
        let should_upsert = match item {
            codex_protocol::items::TurnItem::Plan(plan) => !plan.text.is_empty(),
            codex_protocol::items::TurnItem::HookPrompt(_)
            | codex_protocol::items::TurnItem::FunctionCallOutput(_)
            | codex_protocol::items::TurnItem::CommandExecution(_)
            | codex_protocol::items::TurnItem::DynamicToolCall(_)
            | codex_protocol::items::TurnItem::CollabAgentToolCall(_)
            | codex_protocol::items::TurnItem::SubAgentActivity(_)
            | codex_protocol::items::TurnItem::Extension(_)
            | codex_protocol::items::TurnItem::EnteredReviewMode(_)
            | codex_protocol::items::TurnItem::ExitedReviewMode(_) => true,
            codex_protocol::items::TurnItem::UserMessage(_)
            | codex_protocol::items::TurnItem::AgentMessage(_)
            | codex_protocol::items::TurnItem::Reasoning(_)
            | codex_protocol::items::TurnItem::WebSearch(_)
            | codex_protocol::items::TurnItem::ImageView(_)
            | codex_protocol::items::TurnItem::ImageGeneration(_)
            | codex_protocol::items::TurnItem::FileChange(_)
            | codex_protocol::items::TurnItem::McpToolCall(_)
            | codex_protocol::items::TurnItem::ContextCompaction(_) => false,
        };

        if should_upsert {
            let item = ThreadItem::from(item.clone());
            if is_review_mode_item {
                self.upsert_review_mode_item(Some(turn_id), item);
            } else {
                self.upsert_item_in_turn_id(turn_id, item);
            }
        }
    }

    fn handle_web_search_begin(&mut self, payload: &WebSearchBeginEvent) {
        let item = ThreadItem::WebSearch(WebSearchItem {
            id: payload.call_id.clone(),
            query: String::new(),
            action: None,
            results: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_web_search_end(&mut self, payload: &WebSearchEndEvent) {
        let item = ThreadItem::WebSearch(WebSearchItem {
            id: payload.call_id.clone(),
            query: payload.query.clone(),
            action: Some(web_search_action_from_core(payload.action.clone())),
            results: payload.results.clone(),
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_exec_command_begin(&mut self, payload: &ExecCommandBeginEvent) {
        let item = build_command_execution_begin_item(payload);
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    fn handle_exec_command_end(&mut self, payload: &ExecCommandEndEvent) {
        let item = build_command_execution_end_item(payload);
        // Command completions can arrive out of order. Unified exec may return
        // while a PTY is still running, then emit ExecCommandEnd later from a
        // background exit watcher when that process finally exits. By then, a
        // newer user turn may already have started. Route by event turn_id so
        // replay preserves the original turn association.
        self.upsert_item_in_turn_id(&payload.turn_id, item);
    }

    fn handle_patch_apply_begin(&mut self, payload: &PatchApplyBeginEvent) {
        let item = build_file_change_begin_item(payload);
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_patch_apply_end(&mut self, payload: &PatchApplyEndEvent) {
        let item = build_file_change_end_item(payload);
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_dynamic_tool_call_request(
        &mut self,
        payload: &codex_protocol::dynamic_tools::DynamicToolCallRequest,
    ) {
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status: DynamicToolCallStatus::InProgress,
            content_items: None,
            success: None,
            duration_ms: None,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_dynamic_tool_call_response(&mut self, payload: &DynamicToolCallResponseEvent) {
        let status = if payload.success {
            DynamicToolCallStatus::Completed
        } else {
            DynamicToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let item = ThreadItem::DynamicToolCall {
            id: payload.call_id.clone(),
            namespace: payload.namespace.clone(),
            tool: payload.tool.clone(),
            arguments: payload.arguments.clone(),
            status,
            content_items: Some(convert_dynamic_tool_content_items(&payload.content_items)),
            success: Some(payload.success),
            duration_ms,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_mcp_tool_call_begin(&mut self, payload: &McpToolCallBeginEvent) {
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status: McpToolCallStatus::InProgress,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            mcp_app_ui: payload.mcp_app_ui.clone(),
            plugin_id: payload.plugin_id.clone(),
            read_only_hint: payload.read_only_hint,
            result: None,
            error: None,
            duration_ms: None,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_mcp_tool_call_end(&mut self, payload: &McpToolCallEndEvent) {
        let status = if payload.is_success() {
            McpToolCallStatus::Completed
        } else {
            McpToolCallStatus::Failed
        };
        let duration_ms = i64::try_from(payload.duration.as_millis()).ok();
        let (result, error) = match &payload.result {
            Ok(value) => (
                Some(Box::new(McpToolCallResult {
                    content: value.content.clone(),
                    structured_content: value.structured_content.clone(),
                    meta: value.meta.clone(),
                })),
                None,
            ),
            Err(message) => (
                None,
                Some(McpToolCallError {
                    message: message.clone(),
                }),
            ),
        };
        let item = ThreadItem::McpToolCall {
            id: payload.call_id.clone(),
            server: payload.invocation.server.clone(),
            tool: payload.invocation.tool.clone(),
            status,
            arguments: payload
                .invocation
                .arguments
                .clone()
                .unwrap_or(serde_json::Value::Null),
            app_context: payload
                .connector_id
                .clone()
                .map(|connector_id| McpToolCallAppContext {
                    connector_id,
                    link_id: payload.link_id.clone(),
                    resource_uri: payload.mcp_app_resource_uri.clone(),
                    app_name: payload.app_name.clone(),
                    action_name: payload.action_name.clone(),
                }),
            mcp_app_resource_uri: payload.mcp_app_resource_uri.clone(),
            mcp_app_ui: payload.mcp_app_ui.clone(),
            plugin_id: payload.plugin_id.clone(),
            read_only_hint: payload.read_only_hint,
            result,
            error,
            duration_ms,
        };
        if payload.turn_id.is_empty() {
            self.upsert_item_in_current_turn(item);
        } else {
            self.upsert_item_in_turn_id(&payload.turn_id, item);
        }
    }

    fn handle_view_image_tool_call(&mut self, payload: &ViewImageToolCallEvent) {
        let item = ThreadItem::ImageView {
            id: payload.call_id.clone(),
            path: payload.path.clone().into(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_image_generation_begin(&mut self, payload: &ImageGenerationBeginEvent) {
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: String::new(),
            revised_prompt: None,
            result: String::new(),
            transparent_background: None,
            failure: None,
            saved_path: None,
            imagegen_request_id: None,
            generation_id: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_image_generation_end(&mut self, payload: &ImageGenerationEndEvent) {
        let item = ThreadItem::ImageGeneration(ImageGenerationItem {
            id: payload.call_id.clone(),
            status: payload.status.clone(),
            revised_prompt: payload.revised_prompt.clone(),
            result: payload.result.clone(),
            transparent_background: payload.transparent_background,
            failure: payload.failure.clone(),
            saved_path: payload.saved_path.clone(),
            imagegen_request_id: None,
            generation_id: None,
        });
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_spawn_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnBeginEvent,
    ) {
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: Vec::new(),
            prompt: Some(payload.prompt.clone()),
            model: Some(payload.model.clone()),
            reasoning_effort: Some(payload.reasoning_effort.clone()),
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_spawn_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentSpawnEndEvent,
    ) {
        let has_receiver = payload.new_thread_id.is_some();
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ if has_receiver => CollabAgentToolCallStatus::Completed,
            _ => CollabAgentToolCallStatus::Failed,
        };
        let (receiver_thread_ids, agents_states) = match &payload.new_thread_id {
            Some(id) => {
                let receiver_id = id.to_string();
                let received_status = CollabAgentState::from(payload.status.clone());
                (
                    vec![receiver_id.clone()],
                    [(receiver_id, received_status)].into_iter().collect(),
                )
            }
            None => (Vec::new(), HashMap::new()),
        };
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SpawnAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: Some(payload.prompt.clone()),
            model: Some(payload.model.clone()),
            reasoning_effort: Some(payload.reasoning_effort.clone()),
            agents_states,
        });
    }

    fn handle_collab_agent_interaction_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionBeginEvent,
    ) {
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_agent_interaction_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabAgentInteractionEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        let receiver_id = payload.receiver_thread_id.to_string();
        let received_status = CollabAgentState::from(payload.status.clone());
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::SendInput,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id.clone()],
            prompt: Some(payload.prompt.clone()),
            model: None,
            reasoning_effort: None,
            agents_states: [(receiver_id, received_status)].into_iter().collect(),
        });
    }

    fn handle_sub_agent_activity(
        &mut self,
        payload: &codex_protocol::protocol::SubAgentActivityEvent,
    ) {
        self.upsert_item_in_current_turn(ThreadItem::SubAgentActivity {
            model: payload.model.clone(),
            reasoning_effort: payload.reasoning_effort.clone(),
            id: payload.event_id.clone(),
            kind: payload.kind.into(),
            agent_thread_id: payload.agent_thread_id.to_string(),
            agent_path: String::from(payload.agent_path.clone()),
        });
    }

    fn handle_collab_waiting_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingBeginEvent,
    ) {
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: payload
                .receiver_thread_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_waiting_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabWaitingEndEvent,
    ) {
        let status = if payload
            .statuses
            .values()
            .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
        {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };
        let mut receiver_thread_ids: Vec<String> =
            payload.statuses.keys().map(ToString::to_string).collect();
        receiver_thread_ids.sort();
        let agents_states = payload
            .statuses
            .iter()
            .map(|(id, status)| (id.to_string(), CollabAgentState::from(status.clone())))
            .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::Wait,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids,
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn handle_collab_close_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabCloseBeginEvent,
    ) {
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_close_end(&mut self, payload: &codex_protocol::protocol::CollabCloseEndEvent) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::CloseAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn handle_collab_resume_begin(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeBeginEvent,
    ) {
        let item = ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status: CollabAgentToolCallStatus::InProgress,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![payload.receiver_thread_id.to_string()],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: HashMap::new(),
        };
        self.upsert_item_in_current_turn(item);
    }

    fn handle_collab_resume_end(
        &mut self,
        payload: &codex_protocol::protocol::CollabResumeEndEvent,
    ) {
        let status = match &payload.status {
            AgentStatus::Errored(_) | AgentStatus::NotFound => CollabAgentToolCallStatus::Failed,
            _ => CollabAgentToolCallStatus::Completed,
        };
        let receiver_id = payload.receiver_thread_id.to_string();
        let agents_states = [(
            receiver_id.clone(),
            CollabAgentState::from(payload.status.clone()),
        )]
        .into_iter()
        .collect();
        self.upsert_item_in_current_turn(ThreadItem::CollabAgentToolCall {
            id: payload.call_id.clone(),
            tool: CollabAgentTool::ResumeAgent,
            status,
            sender_thread_id: payload.sender_thread_id.to_string(),
            receiver_thread_ids: vec![receiver_id],
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states,
        });
    }

    fn handle_context_compacted(&mut self, _payload: &ContextCompactedEvent) {
        let id = self.next_item_id();
        self.push_item_in_current_turn(ThreadItem::ContextCompaction { id });
    }

    fn handle_entered_review_mode(
        &mut self,
        payload: &codex_protocol::protocol::EnteredReviewModeEvent,
    ) {
        let review = payload
            .user_facing_hint
            .clone()
            .unwrap_or_else(|| "Review requested.".to_string());
        let id = payload
            .item_id
            .clone()
            .unwrap_or_else(|| self.next_item_id());
        self.upsert_review_mode_item(
            payload.turn_id.as_deref(),
            ThreadItem::EnteredReviewMode { id, review },
        );
    }

    fn handle_exited_review_mode(
        &mut self,
        payload: &codex_protocol::protocol::ExitedReviewModeEvent,
    ) {
        let review = review_output_text(payload.review_output.as_ref());
        let id = payload
            .item_id
            .clone()
            .unwrap_or_else(|| self.next_item_id());
        self.upsert_review_mode_item(
            payload.turn_id.as_deref(),
            ThreadItem::ExitedReviewMode { id, review },
        );
    }

    fn upsert_review_mode_item(&mut self, turn_id: Option<&str>, item: ThreadItem) {
        let Some(turn_id) = turn_id else {
            self.upsert_item_in_current_turn(item);
            return;
        };
        let current_turn_matches = self
            .current_turn
            .as_ref()
            .is_some_and(|turn| turn.id == turn_id);
        if !current_turn_matches && !self.turns.iter().any(|turn| turn.id == turn_id) {
            self.finish_current_turn();
            let turn = self.new_turn(Some(turn_id.to_string()));
            self.record_changed_pending_turn(&turn);
            self.current_turn = Some(turn);
        }
        self.upsert_item_in_turn_id(turn_id, item);
    }

    fn handle_error(&mut self, payload: &ErrorEvent) {
        if !payload.affects_turn_status() {
            return;
        }
        let tracking_changes = self.is_tracking_changes();
        let changed_turn = if let Some(turn) = self.current_turn.as_mut() {
            turn.status = TurnStatus::Failed;
            turn.error = Some(V2TurnError {
                misalignment: payload.misalignment.clone().map(Into::into),
                message: payload.message.clone(),
                codex_error_info: payload.codex_error_info.clone().map(Into::into),
                additional_details: None,
            });
            tracking_changes.then(|| ThreadHistoryTurnMetadata::from_pending_turn(turn))
        } else {
            None
        };
        if let Some(changed_turn) = changed_turn {
            self.record_changed_turn(changed_turn);
        }
    }

    fn handle_turn_aborted(&mut self, payload: &TurnAbortedEvent) {
        let terminal_error = payload.error.as_ref().map(|error| V2TurnError {
            message: error.message.clone(),
            codex_error_info: error.codex_error_info.clone().map(Into::into),
            misalignment: error.misalignment.clone().map(Into::into),
            additional_details: None,
        });
        let apply_abort = |turn: &mut PendingTurn| {
            turn.status = TurnStatus::Interrupted;
            if let Some(error) = terminal_error.as_ref() {
                turn.error = Some(error.clone());
            }
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            ThreadHistoryTurnMetadata::from_pending_turn(turn)
        };
        if let Some(turn_id) = payload.turn_id.as_deref() {
            // Prefer an exact ID match so we interrupt the turn explicitly targeted by the event.
            if let Some(turn) = self.current_turn.as_mut().filter(|turn| turn.id == turn_id) {
                let changed_turn = apply_abort(turn);
                self.record_changed_turn(changed_turn);
                return;
            }

            if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == turn_id) {
                let changed_turn = apply_abort(turn);
                self.record_changed_turn(changed_turn);
                return;
            }
        }

        // If the event has no ID (or refers to an unknown turn), fall back to the active turn.
        if let Some(turn) = self.current_turn.as_mut() {
            let changed_turn = apply_abort(turn);
            self.record_changed_turn(changed_turn);
        }
    }

    fn handle_turn_started(&mut self, payload: &TurnStartedEvent) {
        self.finish_current_turn();
        let mut turn = self
            .new_turn(Some(payload.turn_id.clone()))
            .with_status(TurnStatus::InProgress)
            .with_started_at(payload.started_at)
            .opened_explicitly();
        turn.root_turn_id = payload.root_turn_id.clone();
        self.record_changed_pending_turn(&turn);
        self.current_turn = Some(turn);
    }

    fn handle_turn_complete(&mut self, payload: &TurnCompleteEvent) {
        let terminal_error = payload.error.as_ref().map(|error| V2TurnError {
            misalignment: error.misalignment.clone().map(Into::into),
            message: error.message.clone(),
            codex_error_info: error.codex_error_info.clone().map(Into::into),
            additional_details: None,
        });
        let apply_completion = |turn: &mut PendingTurn| {
            if let Some(error) = terminal_error.as_ref() {
                turn.status = TurnStatus::Failed;
                turn.error = Some(error.clone());
            } else if matches!(turn.status, TurnStatus::Completed | TurnStatus::InProgress) {
                turn.status = TurnStatus::Completed;
            }
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            ThreadHistoryTurnMetadata::from_pending_turn(turn)
        };

        // Prefer an exact ID match from the active turn and then close it.
        if let Some(current_turn) = self
            .current_turn
            .as_mut()
            .filter(|turn| turn.id == payload.turn_id)
        {
            let changed_turn = apply_completion(current_turn);
            self.record_changed_turn(changed_turn);
            self.finish_current_turn();
            return;
        }

        if let Some(turn) = self
            .turns
            .iter_mut()
            .find(|turn| turn.id == payload.turn_id)
        {
            if let Some(error) = terminal_error.as_ref() {
                turn.status = TurnStatus::Failed;
                turn.error = Some(error.clone());
            } else if matches!(turn.status, TurnStatus::Completed | TurnStatus::InProgress) {
                turn.status = TurnStatus::Completed;
            }
            turn.completed_at = payload.completed_at;
            turn.duration_ms = payload.duration_ms;
            let changed_turn = ThreadHistoryTurnMetadata::from_pending_turn(turn);
            self.record_changed_turn(changed_turn);
            return;
        }

        // If the completion event cannot be matched, apply it to the active turn.
        if let Some(current_turn) = self.current_turn.as_mut() {
            let changed_turn = apply_completion(current_turn);
            self.record_changed_turn(changed_turn);
            self.finish_current_turn();
        }
    }

    /// Marks the current turn as containing a persisted compaction marker.
    ///
    /// This keeps compaction-only legacy turns from being dropped by
    /// `finish_current_turn` when they have no renderable items and were not
    /// explicitly opened.
    fn handle_compacted(&mut self, _payload: &CompactedItem) {
        self.ensure_turn().saw_compaction = true;
    }

    fn handle_thread_rollback(&mut self, payload: &ThreadRolledBackEvent) {
        self.finish_current_turn();

        let n = usize::try_from(payload.num_turns).unwrap_or(usize::MAX);
        let removed_turn_ids = if n >= self.turns.len() {
            self.turns.iter().map(|turn| turn.id.clone()).collect()
        } else if n == 0 {
            Vec::new()
        } else {
            self.turns[self.turns.len() - n..]
                .iter()
                .map(|turn| turn.id.clone())
                .collect()
        };
        self.record_removed_turn_ids(removed_turn_ids);

        if n >= self.turns.len() {
            self.turns.clear();
        } else {
            self.turns.truncate(self.turns.len().saturating_sub(n));
        }

        let item_count: usize = self.turns.iter().map(|t| t.items.len()).sum();
        self.next_item_index = i64::try_from(item_count.saturating_add(1)).unwrap_or(i64::MAX);
    }

    fn finish_current_turn(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            if turn.items.is_empty() && !turn.opened_explicitly && !turn.saw_compaction {
                return;
            }
            self.turns.push(turn);
        }
    }

    fn new_turn(&mut self, id: Option<String>) -> PendingTurn {
        let id = id.unwrap_or_else(|| {
            if self.next_rollout_index == 0 {
                Uuid::now_v7().to_string()
            } else {
                format!("rollout-{}", self.current_rollout_index)
            }
        });
        PendingTurn {
            id,
            root_turn_id: None,
            items: Vec::new(),
            item_index: TurnItemIndex::default(),
            error: None,
            status: TurnStatus::Completed,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            opened_explicitly: false,
            saw_compaction: false,
            rollout_start_index: self.current_rollout_index,
        }
    }

    fn ensure_turn(&mut self) -> &mut PendingTurn {
        if self.current_turn.is_none() {
            let turn = self.new_turn(/*id*/ None);
            self.record_changed_pending_turn(&turn);
            self.current_turn = Some(turn);
        }

        if let Some(turn) = self.current_turn.as_mut() {
            return turn;
        }

        unreachable!("current turn must exist after initialization");
    }

    fn push_item_in_current_turn(&mut self, item: ThreadItem) {
        let tracking_changes = self.is_tracking_changes();
        let changed_item = {
            let turn = self.ensure_turn();
            let changed_item = tracking_changes.then(|| (turn.id.clone(), item.clone()));
            turn.item_index.push(&mut turn.items, item);
            changed_item
        };
        if let Some((turn_id, item)) = changed_item {
            self.record_changed_item(turn_id, item);
        }
    }

    fn upsert_item_in_turn_id(&mut self, turn_id: &str, item: ThreadItem) {
        let tracking_changes = self.is_tracking_changes();
        if let Some(turn) = self.current_turn.as_mut()
            && turn.id == turn_id
        {
            let changed_item = {
                let item = turn.item_index.upsert(&mut turn.items, item);
                tracking_changes.then(|| (turn.id.clone(), item.clone()))
            };
            if let Some((turn_id, item)) = changed_item {
                self.record_changed_item(turn_id, item);
            }
            return;
        }

        if let Some(turn) = self.turns.iter_mut().find(|turn| turn.id == turn_id) {
            let changed_item = {
                let item = turn.item_index.upsert(&mut turn.items, item);
                tracking_changes.then(|| (turn.id.clone(), item.clone()))
            };
            if let Some((turn_id, item)) = changed_item {
                self.record_changed_item(turn_id, item);
            }
            return;
        }

        warn!(
            item_id = item.id(),
            "dropping turn-scoped item for unknown turn id `{turn_id}`"
        );
    }

    fn upsert_item_in_current_turn(&mut self, item: ThreadItem) {
        let tracking_changes = self.is_tracking_changes();
        let changed_item = {
            let turn = self.ensure_turn();
            let item = turn.item_index.upsert(&mut turn.items, item);
            tracking_changes.then(|| (turn.id.clone(), item.clone()))
        };
        if let Some((turn_id, item)) = changed_item {
            self.record_changed_item(turn_id, item);
        }
    }

    fn is_tracking_changes(&self) -> bool {
        self.active_change_set.is_some()
    }

    fn record_changed_item(&mut self, turn_id: String, item: ThreadItem) {
        if let Some(change_set) = self.active_change_set.as_mut() {
            change_set.changed_items.push(ThreadHistoryItemChange {
                turn_id,
                item,
                // Legacy events used by ThreadHistoryBuilder don't have timestamps
                started_at_ms: None,
                completed_at_ms: None,
            });
        }
    }

    fn record_changed_pending_turn(&mut self, turn: &PendingTurn) {
        if self.is_tracking_changes() {
            self.record_changed_turn(ThreadHistoryTurnMetadata::from_pending_turn(turn));
        }
    }

    fn record_changed_turn(&mut self, turn: ThreadHistoryTurnMetadata) {
        if let Some(change_set) = self.active_change_set.as_mut() {
            change_set.changed_turns.push(turn);
        }
    }

    fn record_removed_turn_ids(&mut self, removed_turn_ids: Vec<String>) {
        if let Some(change_set) = self.active_change_set.as_mut() {
            change_set.removed_turn_ids.extend(removed_turn_ids);
        }
    }

    fn next_item_id(&mut self) -> String {
        let id = format!("item-{}", self.next_item_index);
        self.next_item_index += 1;
        id
    }

    fn build_user_inputs(&self, payload: &UserMessageEvent) -> Vec<UserInput> {
        let mut content = Vec::new();
        if !payload.message.trim().is_empty() {
            content.push(UserInput::Text {
                text: payload.message.clone(),
                text_elements: payload
                    .text_elements
                    .iter()
                    .cloned()
                    .map(Into::into)
                    .collect(),
            });
        }
        let has_complete_image_order = payload.has_complete_image_order();
        if has_complete_image_order {
            let mut inline_index = 0;
            let mut file_index = 0;
            for image_kind in &payload.image_order {
                let (image, detail) = match image_kind {
                    UserMessageImageKind::Inline => {
                        let Some(image) = payload
                            .images
                            .as_deref()
                            .and_then(|images| images.get(inline_index))
                        else {
                            continue;
                        };
                        let detail = payload.image_details.get(inline_index).copied().flatten();
                        inline_index += 1;
                        (ImageReference::Inline { url: image.clone() }, detail)
                    }
                    UserMessageImageKind::File => {
                        let Some(file_id) = payload
                            .file_ids
                            .as_deref()
                            .and_then(|file_ids| file_ids.get(file_index))
                        else {
                            continue;
                        };
                        let detail = payload.file_id_details.get(file_index).copied().flatten();
                        file_index += 1;
                        (
                            ImageReference::File {
                                file_id: file_id.clone(),
                            },
                            detail,
                        )
                    }
                };
                content.push(UserInput::Image { image, detail });
            }
        } else if let Some(images) = &payload.images {
            for (idx, image) in images.iter().enumerate() {
                content.push(UserInput::Image {
                    image: ImageReference::Inline { url: image.clone() },
                    detail: payload.image_details.get(idx).copied().flatten(),
                });
            }
        }
        if !has_complete_image_order && let Some(file_ids) = &payload.file_ids {
            for (idx, file_id) in file_ids.iter().enumerate() {
                content.push(UserInput::Image {
                    image: ImageReference::File {
                        file_id: file_id.clone(),
                    },
                    detail: payload.file_id_details.get(idx).copied().flatten(),
                });
            }
        }
        for (idx, path) in payload.local_images.iter().enumerate() {
            content.push(UserInput::LocalImage {
                path: path.clone(),
                detail: payload.local_image_details.get(idx).copied().flatten(),
            });
        }
        if let Some(audio) = &payload.audio {
            content.extend(audio.iter().cloned().map(|url| UserInput::Audio { url }));
        }
        content.extend(
            payload
                .local_audio
                .iter()
                .cloned()
                .map(|path| UserInput::LocalAudio { path }),
        );
        content
    }
}

fn convert_dynamic_tool_content_items(
    items: &[codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem],
) -> Vec<DynamicToolCallOutputContentItem> {
    items
        .iter()
        .cloned()
        .map(|item| match item {
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputText { text } => {
                DynamicToolCallOutputContentItem::InputText { text }
            }
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputImage {
                image_url,
            } => DynamicToolCallOutputContentItem::InputImage { image_url },
            codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem::InputAudio {
                audio_url,
            } => DynamicToolCallOutputContentItem::InputAudio { audio_url },
        })
        .collect()
}

const TURN_ITEM_INDEX_THRESHOLD: usize = 32;

/// Lazily indexes a turn's append-only item list. Replacements keep the same ID
/// and position; duplicate IDs continue to resolve to their first occurrence.
/// Keep this index with its turn, including after completion and across rollback.
#[derive(Default)]
struct TurnItemIndex {
    positions: Option<HashMap<String, usize>>,
}

impl TurnItemIndex {
    fn push(&mut self, items: &mut Vec<ThreadItem>, item: ThreadItem) {
        if let Some(positions) = &mut self.positions {
            positions
                .entry(item.id().to_string())
                .or_insert(items.len());
        }
        items.push(item);
    }

    fn upsert<'a>(&mut self, items: &'a mut Vec<ThreadItem>, item: ThreadItem) -> &'a ThreadItem {
        if self.positions.is_none() && items.len() >= TURN_ITEM_INDEX_THRESHOLD {
            let mut positions = HashMap::with_capacity(items.len());
            for (index, existing) in items.iter().enumerate() {
                positions.entry(existing.id().to_string()).or_insert(index);
            }
            self.positions = Some(positions);
        }

        let existing_index = match &self.positions {
            Some(positions) => positions.get(item.id()).copied(),
            None => items.iter().position(|existing| existing.id() == item.id()),
        };
        if let Some(index) = existing_index {
            items[index] = item;
            &items[index]
        } else {
            let index = items.len();
            self.push(items, item);
            &items[index]
        }
    }
}

struct PendingTurn {
    id: String,
    root_turn_id: Option<String>,
    items: Vec<ThreadItem>,
    item_index: TurnItemIndex,
    error: Option<TurnError>,
    status: TurnStatus,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    /// True when this turn originated from an explicit `turn_started`/`turn_complete`
    /// boundary, so we preserve it even if it has no renderable items.
    opened_explicitly: bool,
    /// True when this turn includes a persisted `RolloutItem::Compacted`, which
    /// should keep the turn from being dropped even without normal items.
    saw_compaction: bool,
    /// Index of the rollout item that opened this turn during replay.
    rollout_start_index: usize,
}

impl PendingTurn {
    fn opened_explicitly(mut self) -> Self {
        self.opened_explicitly = true;
        self
    }

    fn with_status(mut self, status: TurnStatus) -> Self {
        self.status = status;
        self
    }

    fn with_started_at(mut self, started_at: Option<i64>) -> Self {
        self.started_at = started_at;
        self
    }

    fn snapshot(&self, items_view: TurnItemsView) -> Turn {
        Turn {
            id: self.id.clone(),
            root_turn_id: self.root_turn_id.clone(),
            items: items_view.project_items(&self.items),
            items_view,
            error: self.error.clone(),
            status: self.status.clone(),
            started_at: self.started_at,
            completed_at: self.completed_at,
            duration_ms: self.duration_ms,
        }
    }
}

impl From<PendingTurn> for Turn {
    fn from(value: PendingTurn) -> Self {
        Self {
            id: value.id,
            root_turn_id: value.root_turn_id,
            items: value.items,
            items_view: TurnItemsView::Full,
            error: value.error,
            status: value.status,
            started_at: value.started_at,
            completed_at: value.completed_at,
            duration_ms: value.duration_ms,
        }
    }
}

impl From<&PendingTurn> for Turn {
    fn from(value: &PendingTurn) -> Self {
        value.snapshot(TurnItemsView::Full)
    }
}

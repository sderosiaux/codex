use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_protocol::models::ResponseItem;
use tokio::sync::Mutex;

use crate::ResponseEvent;
use crate::stream_events_utils::raw_assistant_output_text_from_item;

#[derive(Debug, Default)]
pub(crate) struct TurnTimingState {
    state: Mutex<TurnTimingStateInner>,
}

#[derive(Debug, Default)]
struct TurnTimingStateInner {
    started_at: Option<Instant>,
    started_at_unix_secs: Option<i64>,
    item_started_at_ms: HashMap<String, i64>,
    first_token_at: Option<Instant>,
}

impl TurnTimingState {
    pub(crate) async fn mark_turn_started(&self, started_at: Instant) -> i64 {
        let started_at_unix_ms = now_unix_timestamp_ms();
        let mut state = self.state.lock().await;
        state.started_at = Some(started_at);
        state.started_at_unix_secs = Some(started_at_unix_ms / 1000);
        state.item_started_at_ms.clear();
        state.first_token_at = None;
        started_at_unix_ms
    }

    pub(crate) async fn started_at_unix_secs(&self) -> Option<i64> {
        self.state.lock().await.started_at_unix_secs
    }

    pub(crate) async fn record_item_started(&self, item_id: String, started_at_ms: i64) -> i64 {
        *self
            .state
            .lock()
            .await
            .item_started_at_ms
            .entry(item_id)
            .or_insert(started_at_ms)
    }

    pub(crate) async fn take_item_started(&self, item_id: &str) -> Option<i64> {
        self.state.lock().await.item_started_at_ms.remove(item_id)
    }

    pub(crate) async fn complete_duration_ms(&self) -> (Option<i64>, Option<i64>) {
        let completed_at_instant = Instant::now();
        let state = self.state.lock().await;
        let completed_at = Some(now_unix_timestamp_secs());
        let duration_ms = state.started_at.map(|started_at| {
            i64::try_from(
                completed_at_instant
                    .saturating_duration_since(started_at)
                    .as_millis(),
            )
            .unwrap_or(i64::MAX)
        });
        (completed_at, duration_ms)
    }

    pub(crate) async fn time_to_first_token_ms(&self) -> Option<i64> {
        let state = self.state.lock().await;
        state
            .time_to_first_token()
            .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    }

    pub(crate) async fn record_ttft_for_response_event(&self, event: &ResponseEvent) {
        if response_event_records_turn_ttft(event) {
            self.state.lock().await.record_turn_ttft();
        }
    }
}

fn now_unix_timestamp_secs() -> i64 {
    now_unix_timestamp_ms() / 1000
}

pub(crate) fn now_unix_timestamp_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl TurnTimingStateInner {
    fn time_to_first_token(&self) -> Option<Duration> {
        Some(self.first_token_at?.duration_since(self.started_at?))
    }

    fn record_turn_ttft(&mut self) {
        if self.first_token_at.is_none() && self.started_at.is_some() {
            self.first_token_at = Some(Instant::now());
        }
    }
}

fn response_event_records_turn_ttft(event: &ResponseEvent) -> bool {
    match event {
        ResponseEvent::OutputItemDone(item) | ResponseEvent::OutputItemAdded(item) => {
            response_item_records_turn_ttft(item)
        }
        ResponseEvent::OutputTextDelta(_)
        | ResponseEvent::ReasoningSummaryDelta { .. }
        | ResponseEvent::ReasoningSummaryDone { .. }
        | ResponseEvent::ReasoningContentDelta { .. } => true,
        ResponseEvent::Created { .. }
        | ResponseEvent::ServerModel(_)
        | ResponseEvent::ModelVerifications(_)
        | ResponseEvent::TurnModerationMetadata(_)
        | ResponseEvent::SafetyBuffering(_)
        | ResponseEvent::ServerReasoningIncluded(_)
        | ResponseEvent::ToolCallInputDelta { .. }
        | ResponseEvent::Completed { .. }
        | ResponseEvent::ReasoningSummaryPartAdded { .. }
        | ResponseEvent::RateLimits(_)
        | ResponseEvent::ModelsEtag(_) => false,
    }
}

fn response_item_records_turn_ttft(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { .. } => {
            raw_assistant_output_text_from_item(item).is_some_and(|text| !text.is_empty())
        }
        ResponseItem::Reasoning {
            summary, content, ..
        } => {
            summary.iter().any(|entry| match entry {
                codex_protocol::models::ReasoningItemReasoningSummary::SummaryText { text } => {
                    !text.is_empty()
                }
            }) || content.as_ref().is_some_and(|entries| {
                entries.iter().any(|entry| match entry {
                    codex_protocol::models::ReasoningItemContent::ReasoningText { text }
                    | codex_protocol::models::ReasoningItemContent::Text { text } => {
                        !text.is_empty()
                    }
                })
            })
        }
        ResponseItem::AgentMessage { .. } => false,
        ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ContextCompaction { .. } => true,
        ResponseItem::ConfigurationUpdate { .. } | ResponseItem::CompactionTrigger { .. } => false,
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::Other => false,
    }
}

use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use super::TurnTimingState;
use super::response_item_records_turn_ttft;
use crate::ResponseEvent;

#[tokio::test]
async fn turn_timing_state_records_ttft_only_once_per_turn() {
    let state = TurnTimingState::default();
    assert_eq!(
        state
            .record_ttft_for_response_event(&ResponseEvent::OutputTextDelta("hi".to_string()))
            .await,
        None
    );

    state.mark_turn_started(Instant::now()).await;
    assert_eq!(
        state
            .record_ttft_for_response_event(&ResponseEvent::Created { response_id: None })
            .await,
        None
    );
    assert!(
        state
            .record_ttft_for_response_event(&ResponseEvent::OutputTextDelta("hi".to_string()))
            .await
            .is_some()
    );
    assert_eq!(
        state
            .record_ttft_for_response_event(&ResponseEvent::OutputTextDelta("again".to_string()))
            .await,
        None
    );
}

#[tokio::test]
async fn turn_timing_state_records_ttfm_independently_of_ttft() {
    let state = TurnTimingState::default();
    state.mark_turn_started(Instant::now()).await;

    assert!(
        state
            .record_ttft_for_response_event(&ResponseEvent::OutputTextDelta("hi".to_string()))
            .await
            .is_some()
    );
    assert!(
        state
            .record_ttfm_for_turn_item(&TurnItem::AgentMessage(AgentMessageItem {
                id: "msg-1".to_string(),
                content: Vec::new(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            }))
            .await
            .is_some()
    );
    assert_eq!(
        state
            .record_ttfm_for_turn_item(&TurnItem::AgentMessage(AgentMessageItem {
                id: "msg-2".to_string(),
                content: Vec::new(),
                phase: None,
                memory_citation: None,
                delivery: None,
                questions: None,
            }))
            .await,
        None
    );
}

#[tokio::test]
async fn turn_timing_state_records_turn_started_epoch_millis() {
    let state = TurnTimingState::default();
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();

    let started_at_unix_ms = state.mark_turn_started(Instant::now()).await;

    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    assert!(u128::try_from(started_at_unix_ms).is_ok_and(|ms| before <= ms && ms <= after));
    assert_eq!(
        state.started_at_unix_secs().await,
        Some(started_at_unix_ms / 1000)
    );
}

#[tokio::test]
async fn turn_timing_state_tracks_concurrent_items_and_preserves_first_start() {
    let state = TurnTimingState::default();

    state
        .record_item_started("first".to_string(), /*started_at_ms*/ 100)
        .await;
    state
        .record_item_started("second".to_string(), /*started_at_ms*/ 200)
        .await;
    assert_eq!(
        state
            .record_item_started("first".to_string(), /*started_at_ms*/ 300)
            .await,
        100
    );

    assert_eq!(state.take_item_started("second").await, Some(200));
    assert_eq!(state.take_item_started("first").await, Some(100));
}

#[tokio::test]
async fn turn_timing_state_preserves_in_flight_items_after_turn_completion() {
    let state = TurnTimingState::default();
    state
        .record_item_started("first".to_string(), /*started_at_ms*/ 100)
        .await;

    state.mark_turn_started(Instant::now()).await;
    assert_eq!(state.take_item_started("first").await, None);

    state
        .record_item_started("second".to_string(), /*started_at_ms*/ 200)
        .await;
    let _ = state.complete_duration_ms().await;
    assert_eq!(state.take_item_started("second").await, Some(200));
}

#[test]
fn response_item_records_turn_ttft_for_first_output_signals() {
    assert!(response_item_records_turn_ttft(
        &ResponseItem::FunctionCall {
            status: None,
            encrypted_content: None,
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
        }
    ));
    assert!(response_item_records_turn_ttft(
        &ResponseItem::CustomToolCall {
            encrypted_content: None,
            id: None,
            status: None,
            call_id: "call-2".to_string(),
            name: "custom".to_string(),
            namespace: None,
            input: "echo hi".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
    ));
    assert!(response_item_records_turn_ttft(&ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: "hello".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }));
}

#[test]
fn response_item_records_turn_ttft_ignores_empty_non_output_items() {
    assert!(!response_item_records_turn_ttft(&ResponseItem::Message {
        status: None,
        encrypted_content: None,
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            annotations: None,
            logprobs: None,
            text: String::new(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }));
    assert!(!response_item_records_turn_ttft(
        &ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call-1".to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        }
    ));
}

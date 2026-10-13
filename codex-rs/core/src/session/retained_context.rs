//! Orders retained inputs at acceptance and assistant messages at stream start.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_protocol::models::ResponseItem;

use super::Session;
use super::TurnContext;

/// Only unrecorded starts need a reservation; abandoned entries expire with the turn.
#[derive(Default)]
pub(super) struct PendingAssistantMessageOrders(pub(super) Mutex<HashMap<String, u64>>);

impl Session {
    /// Reserve before deriving display items, including plans, from the source message.
    /// Completion-only responses use the same path before publishing their text.
    pub(super) async fn reserve_assistant_message_order(
        &self,
        turn_context: &TurnContext,
        item: &ResponseItem,
    ) {
        if let ResponseItem::Message {
            id: Some(id), role, ..
        } = item
            && role == "assistant"
        {
            let mut state = self.state.lock().await;
            if !state
                .history
                .raw_items()
                .any(|item| item.id().is_some_and(|recorded_id| recorded_id == id))
            {
                turn_context
                    .extension_data
                    .get_or_init(PendingAssistantMessageOrders::default)
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(id.to_string())
                    .or_insert_with(|| state.history.reserve_input_order());
            }
        }
    }

    pub(crate) async fn reserve_user_input_order(&self) -> u64 {
        self.state.lock().await.history.reserve_input_order()
    }
}

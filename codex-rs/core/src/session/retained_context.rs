//! Orders retained inputs at acceptance and assistant messages at stream start.
//! Explicit goal authorization becomes live only after its persistence barrier succeeds.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::context::ContextualUserFragment;
use crate::context::UserGoalUpdate;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;

use super::Session;
use super::TurnContext;
use super::thread_settings;

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

    /// Records authorization without adding pending input or reopening an active turn.
    pub(crate) async fn record_user_goal_update(
        &self,
        update: UserGoalUpdate,
    ) -> std::io::Result<()> {
        // Goal metadata must not initialize a model step, even on a goal-first thread.
        // Keep context construction off callers' stacks, including the TUI RPC dispatcher.
        let context = Box::pin(self.new_inject_items_context()).await;
        let user_input_order = self.reserve_user_input_order().await;
        let _guard = thread_settings::acquire_persistence_lock(self).await;
        let mut item = ContextualUserFragment::into(update);
        Self::stamp_response_item_for_history(&mut item, &context.sub_id);
        Self::assign_missing_response_item_id(&mut item);
        let mut item = ResponseItemEnvelope {
            item,
            metadata: Some(CodexHarnessMetadata {
                user_input_order: Some(user_input_order),
                ..Default::default()
            }),
        };
        if let Some(live_thread) = self.live_thread() {
            // Capture settings under the same permit as the instruction, including when a
            // goal creates the rollout. No fallible settings catch-up runs after publication.
            live_thread
                .append_items(&[
                    RolloutItem::EventMsg(thread_settings::applied_event(self).await),
                    RolloutItem::ResponseItem(item.clone()),
                ])
                .await
                .map_err(std::io::Error::other)?;
        }
        // A failed append or checkpoint must not change live authorization or its revision.
        self.state.lock().await.history.record_annotated_items(
            std::slice::from_mut(&mut item),
            context.model_info().truncation_policy.into(),
        );
        self.send_raw_response_items(&context, std::slice::from_ref(&item.item))
            .await;
        Ok(())
    }

    pub(crate) async fn reserve_user_input_order(&self) -> u64 {
        self.state.lock().await.history.reserve_input_order()
    }
}

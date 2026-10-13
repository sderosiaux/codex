use std::collections::HashMap;

use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use tokio::sync::watch;

use super::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;

async fn wait_until_held(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| *paused)
        .await
        .expect("elicitation service should remain available");
}

async fn wait_until_released(pause_state: &mut watch::Receiver<bool>) {
    pause_state
        .wait_for(|paused| !*paused)
        .await
        .expect("elicitation service should remain available");
}

#[tokio::test]
async fn request_user_input_holds_an_elicitation_until_response() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let mut pause_state = session.subscribe_elicitation_pause_state();

    let request = tokio::spawn({
        let session = session.clone();
        let turn_context = turn_context.clone();
        async move {
            session
                .request_user_input(
                    turn_context.as_ref(),
                    "call-1".to_string(),
                    RequestUserInputArgs {
                        questions: Vec::new(),
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await
        }
    });

    events.recv().await.expect("request user input event");
    wait_until_held(&mut pause_state).await;

    let response = RequestUserInputResponse {
        answers: HashMap::new(),
    };
    session
        .notify_user_input_response(&turn_context.sub_id, response)
        .await;

    request.await.expect("request user input task");
    wait_until_released(&mut pause_state).await;
}

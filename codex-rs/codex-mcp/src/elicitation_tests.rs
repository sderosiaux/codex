use super::*;
use async_channel::Receiver;
use pretty_assertions::assert_eq;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitationSchema;
use serde_json::Value;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

#[tokio::test]
async fn abandonment_is_ordered_and_exactly_once_when_the_native_waiter_is_dropped() {
    let (manager, _, _) = fixture(/*lifecycle*/ None);
    let router = manager.router.clone();
    let authority = manager.authority.lock().unwrap().clone().unwrap();
    let (tx, events) = async_channel::unbounded();
    let (replacement_tx, replacement_events) = async_channel::unbounded();
    let request = ElicitationRequest::Form {
        meta: None,
        message: "Continue".into(),
        requested_schema: json!({"type": "object"}),
    };
    let mut first = Box::pin(router.request_user_interaction(
        Some(tx),
        &authority,
        "server".into(),
        request.clone(),
    ));
    let mut sibling = Box::pin(router.request_user_interaction(
        Some(replacement_tx),
        &authority,
        "server".into(),
        request,
    ));
    assert!(futures::poll!(&mut first).is_pending());
    let EventMsg::ElicitationRequest(first_request) = events.recv().await.unwrap().msg else {
        panic!("request must precede abandonment");
    };
    assert!(futures::poll!(&mut sibling).is_pending());
    let EventMsg::ElicitationRequest(sibling_request) =
        replacement_events.recv().await.unwrap().msg
    else {
        panic!("expected sibling request");
    };
    drop(first);
    let EventMsg::ElicitationAbandoned(abandoned) = events.recv().await.unwrap().msg else {
        panic!("expected abandonment");
    };
    assert_eq!(
        abandoned,
        ElicitationAbandonedEvent {
            server_name: first_request.server_name,
            id: first_request.id.clone()
        }
    );
    assert_eq!(router.requests.lock().unwrap().pending.len(), 1);
    let ProtocolRequestId::String(id) = first_request.id else {
        panic!("generated token")
    };
    assert!(
        router
            .resolve(
                "server".into(),
                RequestId::String(id.into()),
                approved_response()
            )
            .await
            .is_err()
    );
    let ProtocolRequestId::String(id) = sibling_request.id else {
        panic!("generated token")
    };
    router
        .resolve(
            "server".into(),
            RequestId::String(id.into()),
            approved_response(),
        )
        .await
        .unwrap();
    assert_eq!(sibling.await.unwrap(), approved_response());
    assert!(events.try_recv().is_err());
    assert!(replacement_events.try_recv().is_err());
    assert!(router.requests.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn bounded_sources_preserve_backpressure_across_close() {
    let (manager, _, _) = fixture(/*lifecycle*/ None);
    let authority = manager.authority.lock().unwrap().clone().unwrap();
    let router = manager.router.clone();
    let (tx, events) = async_channel::bounded(1);
    tx.send(Event {
        id: "queued".into(),
        msg: EventMsg::ShutdownComplete,
    })
    .await
    .unwrap();
    let mut pending = Box::pin(router.request_user_interaction(
        Some(tx),
        &authority,
        "server".into(),
        ElicitationRequest::Form {
            meta: None,
            message: "Continue".into(),
            requested_schema: json!({"type": "object"}),
        },
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    router.close().await;
    assert_eq!(router.requests.lock().unwrap().pending.len(), 1);
    assert_eq!(events.recv().await.unwrap().id, "queued");
    assert!(futures::poll!(&mut pending).is_pending());
    let EventMsg::ElicitationRequest(request) = events.recv().await.unwrap().msg else {
        panic!("bounded request must publish after capacity becomes available");
    };
    let ProtocolRequestId::String(id) = request.id else {
        panic!("expected generated token");
    };
    router
        .resolve(
            "server".into(),
            RequestId::String(id.into()),
            approved_response(),
        )
        .await
        .unwrap();
    assert_eq!(pending.await.unwrap(), approved_response());
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn abandonment_closed_receiver_cleans_up_without_delivery_or_replay() {
    let (manager, _, _) = fixture(/*lifecycle*/ None);
    let authority = manager.authority.lock().unwrap().clone().unwrap();
    let router = manager.router.clone();
    for close_before_publication in [true, false] {
        let (tx, events) = async_channel::unbounded();
        if close_before_publication {
            events.close();
        }
        let mut pending = Box::pin(router.request_user_interaction(
            Some(tx),
            &authority,
            "server".into(),
            ElicitationRequest::Form {
                meta: None,
                message: "Continue".into(),
                requested_schema: json!({"type": "object"}),
            },
        ));
        if close_before_publication {
            assert!(pending.await.is_err());
        } else {
            assert!(futures::poll!(&mut pending).is_pending());
            assert!(matches!(
                events.recv().await.unwrap().msg,
                EventMsg::ElicitationRequest(_)
            ));
            events.close();
            drop(pending);
        }
        assert!(router.requests.lock().unwrap().pending.is_empty());
        assert!(events.try_recv().is_err());
    }
}

#[tokio::test]
async fn abandonment_close_joins_elected_actions_and_rejects_retained_router_admission() {
    let (manager, _, _) = fixture(/*lifecycle*/ None);
    let authority = manager.authority.lock().unwrap().clone().unwrap();
    let router = manager.router.clone();
    let retained_router = router.clone();
    let (tx, events) = async_channel::unbounded();
    let request = ElicitationRequest::Form {
        meta: None,
        message: "Continue".into(),
        requested_schema: json!({"type": "object"}),
    };
    let mut pending = Box::pin(router.request_user_interaction(
        Some(tx.clone()),
        &authority,
        "server".into(),
        request.clone(),
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    let EventMsg::ElicitationRequest(published) = events.recv().await.unwrap().msg else {
        panic!("expected published request");
    };
    let ProtocolRequestId::String(id) = &published.id else {
        panic!("expected generated token");
    };
    let key = (
        published.server_name.clone(),
        RequestId::String(id.clone().into()),
    );
    let (responder, terminal_action) = router.take_responder(&key).unwrap().unwrap();
    let mut close = Box::pin(router.close());
    assert!(futures::poll!(&mut close).is_pending());
    drop(close);
    let mut repeated_close = Box::pin(retained_router.close());
    assert!(futures::poll!(&mut repeated_close).is_pending());
    assert!(
        retained_router
            .request_user_interaction(
                Some(tx.clone()),
                &authority,
                "server".into(),
                request.clone(),
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("router closed")
    );
    assert!(events.try_recv().is_err());
    emit_abandonment(&key, responder.abandonment_events.as_ref());
    drop(responder);
    drop(terminal_action);
    repeated_close.await;
    assert!(pending.await.is_err());
    let EventMsg::ElicitationAbandoned(abandoned) = events.recv().await.unwrap().msg else {
        panic!("close must wait for the winning remover to enqueue abandonment");
    };
    assert_eq!(
        abandoned,
        ElicitationAbandonedEvent {
            server_name: published.server_name,
            id: published.id,
        }
    );
    router.close().await;
    assert!(
        retained_router
            .request_user_interaction(Some(tx), &authority, "server".into(), request,)
            .await
            .is_err()
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn abandonment_close_invalidates_a_published_responder_before_returning() {
    let (manager, _, _) = fixture(/*lifecycle*/ None);
    let authority = manager.authority.lock().unwrap().clone().unwrap();
    let router = manager.router.clone();
    let (tx, events) = async_channel::unbounded();
    let mut pending = Box::pin(router.request_user_interaction(
        Some(tx),
        &authority,
        "server".into(),
        ElicitationRequest::Form {
            meta: None,
            message: "Continue".into(),
            requested_schema: json!({"type": "object"}),
        },
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    let EventMsg::ElicitationRequest(published) = events.recv().await.unwrap().msg else {
        panic!("expected request");
    };
    router.close().await;
    assert!(pending.await.is_err());
    let EventMsg::ElicitationAbandoned(abandoned) = events.try_recv().unwrap().msg else {
        panic!("close must enqueue abandonment before returning");
    };
    assert_eq!(
        abandoned,
        ElicitationAbandonedEvent {
            server_name: published.server_name,
            id: published.id,
        }
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn abandonment_resolve_reports_failed_response_delivery_once() {
    let router = ElicitationRequestRouter::default();
    let (tx, events) = async_channel::unbounded();
    let (response, receiver) = tokio::sync::oneshot::channel();
    let key = (
        "server".to_string(),
        RequestId::String("published-token".into()),
    );
    router.requests.lock().unwrap().pending.insert(
        key.clone(),
        ElicitationResponder {
            response,
            abandonment_events: Some(tx),
        },
    );
    drop(receiver);
    assert!(
        router
            .resolve(key.0, key.1, approved_response())
            .await
            .is_err()
    );
    router.close().await;
    let EventMsg::ElicitationAbandoned(abandoned) = events.try_recv().unwrap().msg else {
        panic!("failed oneshot delivery must abandon the published request");
    };
    assert_eq!(
        abandoned,
        ElicitationAbandonedEvent {
            server_name: "server".into(),
            id: ProtocolRequestId::String("published-token".into()),
        }
    );
    assert!(events.try_recv().is_err());
}

struct LifecycleRegistration(Arc<AtomicUsize>);

impl Drop for LifecycleRegistration {
    fn drop(&mut self) {
        self.0.fetch_sub(/*val*/ 1, Relaxed);
    }
}

fn approved_response() -> ElicitationResponse {
    ElicitationResponse {
        action: ElicitationAction::Accept,
        content: Some(json!({})),
        meta: None,
    }
}

fn fixture(
    lifecycle: Option<ElicitationLifecycle>,
) -> (ElicitationRequestManager, Receiver<Event>, SendElicitation) {
    let manager = ElicitationRequestManager::new(lifecycle, ElicitationRequestRouter::default());
    let (tx_event, events) = async_channel::bounded(1);
    let sender = manager.make_sender("independent-mcp".to_string(), Some(tx_event));
    (manager, events, sender)
}

fn form_elicitations(properties: Value) -> [Elicitation; 3] {
    let schema = json!({"type": "object", "properties": properties});
    [
        Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: "Review this request".into(),
            requested_schema: serde_json::from_value(schema.clone()).unwrap(),
        }),
        Elicitation::OpenAiForm {
            meta: None,
            message: "Review this request".into(),
            requested_schema: schema.clone(),
        },
        Elicitation::OpenAiElicitationForm {
            meta: None,
            message: "Review this request".into(),
            requested_schema: schema,
        },
    ]
}

#[tokio::test]
async fn confirmation_only_elicitations_are_accepted_without_prompting() {
    let (manager, events, sender) = fixture(/*lifecycle*/ None);
    let elicitation = Elicitation::Mcp(ElicitRequestParams::FormElicitationParams {
        meta: None,
        message: "Run this tool?".to_string(),
        requested_schema: ElicitationSchema::builder().build().unwrap(),
    });

    assert_eq!(
        sender(RequestId::Number(7), elicitation).await.unwrap(),
        approved_response()
    );
    assert!(events.is_empty());
    assert!(manager.router.requests.lock().unwrap().pending.is_empty());
}

#[tokio::test]
async fn auto_deny_declines_without_prompting() {
    let (manager, events, sender) = fixture(/*lifecycle*/ None);
    manager.router.set_auto_deny(/*auto_deny*/ true);
    let [elicitation, _, _] = form_elicitations(json!({"answer": {"type": "string"}}));

    assert_eq!(
        sender(RequestId::Number(7), elicitation).await.unwrap(),
        ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        }
    );
    assert!(events.is_empty());
}

#[tokio::test]
async fn elicitations_needing_input_prompt_the_client() {
    let active_elicitations = Arc::new(AtomicUsize::new(0));
    let registrations = active_elicitations.clone();
    let lifecycle = ElicitationLifecycle::new(move || {
        registrations.fetch_add(/*val*/ 1, Relaxed);
        LifecycleRegistration(registrations.clone())
    });
    let (manager, events, sender) = fixture(Some(lifecycle));
    let [_, openai_form, openai_elicitation_form] = form_elicitations(json!({}));
    let mut requests = vec![openai_form, openai_elicitation_form];
    requests.extend(form_elicitations(json!({"answer": {"type": "string"}})));
    requests.push(Elicitation::Mcp(
        ElicitRequestParams::UrlElicitationParams {
            meta: None,
            message: "Sign in to continue".into(),
            url: "https://example.com/auth".into(),
            elicitation_id: "test-auth".into(),
        },
    ));
    for elicitation in requests {
        let pending = tokio::spawn(sender(RequestId::Number(7), elicitation));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("elicitation should prompt")
            .expect("event channel should remain open");
        let EventMsg::ElicitationRequest(request) = event.msg else {
            panic!("expected MCP elicitation");
        };
        assert!(!pending.is_finished());
        assert_eq!(active_elicitations.load(Relaxed), 1);
        let ProtocolRequestId::String(request_id) = request.id else {
            panic!("expected Codex-owned string request ID");
        };
        let response = ElicitationResponse {
            action: ElicitationAction::Accept,
            content: Some(json!({"answer": "continue"})),
            meta: None,
        };
        manager
            .router
            .resolve(
                request.server_name,
                RequestId::String(request_id.into()),
                response.clone(),
            )
            .await
            .unwrap();
        assert_eq!(pending.await.unwrap().unwrap(), response);
        assert_eq!(active_elicitations.load(Relaxed), 0);
    }
    assert!(events.is_empty());
    assert!(manager.router.requests.lock().unwrap().pending.is_empty());
}

#[test]
fn closed_event_channel_immediately_cleans_up_pending_elicitation() {
    let active_elicitations = Arc::new(AtomicUsize::new(0));
    let registrations = active_elicitations.clone();
    let lifecycle = ElicitationLifecycle::new(move || {
        registrations.fetch_add(/*val*/ 1, Relaxed);
        LifecycleRegistration(registrations.clone())
    });
    let (manager, events, sender) = fixture(/*lifecycle*/ None);
    assert!(manager.update(Some(lifecycle)));
    drop(events);

    let [elicitation, _, _] = form_elicitations(json!({"answer": {"type": "string"}}));
    let error = sender(RequestId::Number(7), elicitation)
        .now_or_never()
        .expect("closed event channel must not leave an elicitation pending")
        .expect_err("closed event channel must fail the elicitation");

    assert_eq!(
        error.to_string(),
        "failed to deliver MCP elicitation request"
    );
    assert!(
        manager
            .router
            .requests
            .lock()
            .expect("pending request router should be available")
            .pending
            .is_empty()
    );
    assert_eq!(active_elicitations.load(Relaxed), 0);
}

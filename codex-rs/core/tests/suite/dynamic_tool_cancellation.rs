//! Dynamic-call cancellation settles live events, persisted history, and model context.

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::hooks::enable_discovered_hooks;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default)]
struct ToolOutcomes(Mutex<Vec<ToolCallOutcome>>);

impl ToolLifecycleContributor for ToolOutcomes {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.0
                .lock()
                .expect("tool outcomes lock poisoned")
                .push(input.outcome);
        })
    }
}

#[test_case::test_case(None; "handler")]
#[test_case::test_case(Some(HookEventName::PreToolUse); "pre_hook")]
#[test_case::test_case(Some(HookEventName::PostToolUse); "post_hook")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_completes_cancelled_dynamic_call(hook: Option<HookEventName>) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let outcomes = Arc::new(ToolOutcomes::default());
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.tool_lifecycle_contributor(outcomes.clone());
    let test =
        test_codex()
            .with_extensions(Arc::new(extensions.build()))
            .with_pre_build_hook(move |home| {
                if let Some(hook) = hook {
                    let name = match hook {
                        HookEventName::PreToolUse => "PreToolUse",
                        HookEventName::PostToolUse => "PostToolUse",
                        _ => unreachable!(),
                    };
                    std::fs::write(home.join("hooks.json"), json!({"hooks": {
                    name: [{"matcher": "^gate$", "hooks": [{"type": "command",
                        "command": "python3 -c 'import threading; threading.Event().wait()'"}]}]
                }}).to_string()).expect("write blocking hook fixture");
                }
            })
            .with_config(move |config| {
                if hook.is_some() {
                    enable_discovered_hooks(config);
                }
            })
            .build_with_auto_env(&server)
            .await?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
                name: "gate".to_string(),
                description: "Wait for a host reply.".to_string(),
                input_schema: json!({"type": "object"}),
                defer_loading: false,
            })],
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("run"),
            ev_function_call("gate-call", "gate", "{}"),
            ev_completed("run"),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "start a call".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    if hook != Some(HookEventName::PreToolUse) {
        wait_for_event(&thread, |event| {
            matches!(event, EventMsg::DynamicToolCallRequest(_))
        })
        .await;
    }
    if hook == Some(HookEventName::PostToolUse) {
        thread
            .submit(Op::DynamicToolResponse {
                id: "gate-call".to_string(),
                response: DynamicToolResponse {
                    content_items: Vec::new(),
                    success: true,
                },
            })
            .await?;
    }
    if let Some(hook) = hook {
        wait_for_event(
            &thread,
            |event| matches!(event, EventMsg::HookStarted(event) if event.run.event_name == hook),
        )
        .await;
    }
    thread.submit(Op::Interrupt).await?;
    if hook.is_none() {
        let item = wait_for_event_match(&thread, |event| match event {
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::DynamicToolCall(item) => Some(item.clone()),
                _ => None,
            },
            EventMsg::TurnAborted(_) => panic!("turn aborted before the dynamic call completed"),
            _ => None,
        })
        .await;
        assert_eq!(
            (item.id.as_str(), item.status, item.success, item.error),
            (
                "gate-call",
                DynamicToolCallStatus::Failed,
                Some(false),
                Some("dynamic tool call was cancelled before receiving a response".to_string())
            )
        );
    }
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnAborted(_))).await;
    // Forced turn abort would drop dispatch without its tool callback. Cancellation
    // must finish before that fallback even while the hook stays blocked.
    assert_eq!(
        *outcomes.0.lock().expect("tool outcomes lock poisoned"),
        vec![ToolCallOutcome::Aborted]
    );
    Ok(())
}

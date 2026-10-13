#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_models_manager::bundled_models_response;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::items::McpAppDisplayMode;
use codex_protocol::items::McpAppUi;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnSettingsUpdate;
use codex_protocol::protocol::TurnSettingsUpdateOutcome;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::apps_test_server::SEARCH_CALENDAR_CREATE_TOOL;
use core_test_support::apps_test_server::SEARCH_CALENDAR_NAMESPACE;
use core_test_support::apps_test_server::recorded_apps_tool_call_by_call_id;
use core_test_support::apps_test_server::search_capable_apps_builder;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use test_case::test_case;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(json!({"ui": {"resourceUri": "ui://calendar/widget"}, "openai/ui": {"preferredModelDisplayMode": "fullscreen"}}), Some(McpAppDisplayMode::Fullscreen), Some("ui://calendar/widget"); "fullscreen")]
#[test_case(json!({"ui": {"resourceUri": "ui://calendar/widget"}, "openai/ui": {"preferredModelDisplayMode": "inline"}}), Some(McpAppDisplayMode::Inline), Some("ui://calendar/widget"); "inline")]
#[test_case(json!({"ui": {"resourceUri": "ui://calendar/widget"}}), None, Some("ui://calendar/widget"); "missing preference")]
#[test_case(json!({"ui": {"resourceUri": "ui://calendar/widget"}, "openai/ui": {"preferredModelDisplayMode": "unsupported"}}), None, Some("ui://calendar/widget"); "unsupported preference")]
#[test_case(json!({"openai/outputTemplate": "ui://calendar/widget"}), None, Some("ui://calendar/widget"); "legacy uri")]
#[test_case(json!({}), None, None; "result only ui")]
async fn mcp_app_ui_survives_tool_events_and_resume(
    mut metadata: Value,
    expected_mode: Option<McpAppDisplayMode>,
    expected_uri: Option<&str>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    metadata["connector_id"] = json!("calendar");
    metadata["connector_name"] = json!("Calendar");
    Mock::given(method("POST"))
        .and(path_regex("^/api/codex/ps/mcp/?$"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("valid tools/list");
            ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": {"tools": [{
                    "name": "calendar_create_event",
                    "description": "Create a calendar event.",
                    "inputSchema": {"type": "object", "properties": {}},
                    "annotations": {"readOnlyHint": true},
                    "_meta": metadata
                }]}
            }))
        })
        .with_priority(/*priority*/ 1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/api/codex/ps/mcp/?$"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("valid tools/call");
            ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": {
                    "content": [{"type": "text", "text": "Calendar opened."}],
                    "_meta": {"openai/outputTemplate": "ui://calendar/result-widget"},
                    "isError": false
                }
            }))
        })
        .with_priority(/*priority*/ 1)
        .mount(&server)
        .await;
    let call_id = "calendar-ui";
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-calendar"),
                ev_function_call_with_namespace(
                    call_id,
                    SEARCH_CALENDAR_NAMESPACE,
                    SEARCH_CALENDAR_CREATE_TOOL,
                    "{}",
                ),
                ev_completed("resp-calendar"),
            ]),
            sse(vec![
                ev_assistant_message("msg-done", "done"),
                ev_completed("resp-done"),
            ]),
        ],
    )
    .await;
    let mut builder = search_capable_apps_builder(apps_server.chatgpt_base_url);
    let test = builder.build_with_auto_env(&server).await?;
    submit_user_turn(
        &test,
        "Use [$calendar](app://calendar) to open the calendar widget.",
        /*collaboration_mode*/ None,
    )
    .await?;

    let expected_ui = expected_mode.map(|preferred_model_display_mode| McpAppUi {
        resource_uri: "ui://calendar/widget".to_string(),
        preferred_model_display_mode,
    });
    let expected_uri = expected_uri.map(str::to_string);
    let mut observed = Vec::new();
    let mut completed = None;
    wait_for_event(&test.codex, |event| {
        match event {
            EventMsg::ItemStarted(ItemStartedEvent {
                item: TurnItem::McpToolCall(item),
                ..
            })
            | EventMsg::ItemCompleted(ItemCompletedEvent {
                item: TurnItem::McpToolCall(item),
                ..
            }) => {
                observed.push((item.mcp_app_ui.clone(), item.mcp_app_resource_uri.clone()));
            }
            EventMsg::McpToolCallBegin(begin) => {
                observed.push((begin.mcp_app_ui.clone(), begin.mcp_app_resource_uri.clone()));
            }
            EventMsg::McpToolCallEnd(end) => {
                assert!(end.result.is_ok());
                observed.push((end.mcp_app_ui.clone(), end.mcp_app_resource_uri.clone()));
                completed = Some(event.clone());
            }
            _ => {}
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(observed, vec![(expected_ui, expected_uri); 4]);
    assert_eq!(responses.requests().len(), 2);

    let resumed = builder.restart(&server, &test).await?;
    let completed_history = resumed
        .codex
        .load_history(/*include_archived*/ false)
        .await?
        .items
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::McpToolCallEnd(end)) if end.call_id == call_id => {
                Some(EventMsg::McpToolCallEnd(end))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(completed_history)?,
        serde_json::to_value(vec![completed.expect("completed MCP call")])?
    );
    Ok(())
}

async fn submit_user_turn(
    test: &TestCodex,
    text: &str,
    collaboration_mode: Option<CollaborationMode>,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![test.executor_environment().request()],
                )),
                collaboration_mode: collaboration_mode.or({
                    Some(CollaborationMode {
                        mode: ModeKind::Default,
                        settings: Settings {
                            model: session_model,
                            reasoning_effort: None,
                            developer_instructions: None,
                        },
                    })
                }),
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

fn attribution_models(model_slugs: [&str; 2]) -> Vec<codex_protocol::openai_models::ModelInfo> {
    let base_model = bundled_models_response()
        .expect("bundled models should parse")
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-5.5")
        .expect("bundled gpt-5.5 model");
    model_slugs
        .into_iter()
        .map(|slug| {
            let mut model = base_model.clone();
            model.slug = slug.to_string();
            model
        })
        .collect()
}

async fn apply_turn_attribution_update(
    test: &TestCodex,
    request_user_input_call_id: &str,
    model: &str,
) -> Result<()> {
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    assert_eq!(request.call_id, request_user_input_call_id);

    let (reply, outcome) = tokio::sync::oneshot::channel();
    test.codex
        .submit(Op::TurnSettings {
            turn_id: request.turn_id.clone(),
            update: TurnSettingsUpdate {
                model: Some(model.to_string()),
                effort: Some(Some(ReasoningEffort::High)),
                ..Default::default()
            },
            reply,
        })
        .await?;
    assert_eq!(outcome.await?, TurnSettingsUpdateOutcome::Applied);

    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "confirm_path".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes (Recommended)".to_string()],
                    },
                )]),
            },
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_tool_call_metadata_uses_captured_step_after_request_user_input() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let apps_server = AppsTestServer::mount(&server).await?;
    let request_user_input_call_id = "user-input-call";
    let calendar_call_id = "calendar-call-after-user-input";
    let request_user_input_args = json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    })
    .to_string();
    let calendar_args = serde_json::to_string(&json!({
        "title": "Lunch",
        "starts_at": "2026-03-10T12:00:00Z"
    }))?;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    request_user_input_call_id,
                    "request_user_input",
                    &request_user_input_args,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call_with_namespace(
                    calendar_call_id,
                    SEARCH_CALENDAR_NAMESPACE,
                    SEARCH_CALENDAR_CREATE_TOOL,
                    &calendar_args,
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let model_a = "mcp-metadata-a";
    let model_b = "mcp-metadata-b";
    let models = attribution_models([model_a, model_b]);
    let mut builder = search_capable_apps_builder(apps_server.chatgpt_base_url.clone())
        .with_model(model_a)
        .with_config(move |config| {
            config.features.enable(Feature::StepModelSwitching);
            config.model_catalog = Some(ModelsResponse { models });
            config.model_reasoning_effort = Some(ReasoningEffort::Low);
        });
    let test = builder.build(&server).await?;

    submit_user_turn(
        &test,
        "Ask for confirmation, then create a calendar event.",
        Some(CollaborationMode {
            mode: ModeKind::Plan,
            settings: Settings {
                model: test.session_configured.model.clone(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        }),
    )
    .await?;

    apply_turn_attribution_update(&test, request_user_input_call_id, model_b).await?;

    let EventMsg::McpToolCallBegin(begin) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::McpToolCallBegin(_))
    })
    .await
    else {
        unreachable!("event guard guarantees McpToolCallBegin");
    };
    assert_eq!(begin.call_id, calendar_call_id);

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(mock.requests().len(), 3);
    let apps_tool_call = recorded_apps_tool_call_by_call_id(&server, calendar_call_id).await;

    assert_eq!(
        apps_tool_call
            .pointer("/params/_meta/x-codex-turn-metadata/user_input_requested_during_turn"),
        Some(&json!(true))
    );
    assert_eq!(
        apps_tool_call.pointer("/params/_meta/x-codex-turn-metadata/model"),
        Some(&json!(model_b))
    );
    assert_eq!(
        apps_tool_call.pointer("/params/_meta/x-codex-turn-metadata/reasoning_effort"),
        Some(&json!("high"))
    );

    Ok(())
}

#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_custom_tool_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::ResponseTemplate;

use super::direct_tool_metadata::tool_call_metadata;

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test_case(false, false; "normal sampling")]
#[test_case(true, false; "pre sampling compaction")]
#[test_case(false, true; "namespace collision")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_tool_collisions_fail_the_turn_before_sampling(
    pre_compact: bool,
    namespace_collision: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(move |config| {
        config.tool_registry.error_on_tool_collisions = true;
        config.update_plan_enabled = true;
        if pre_compact {
            config.model_auto_compact_token_limit = Some(0);
        }
    });
    let test = builder.build_with_auto_env(&server).await?;
    let dynamic_tools = if namespace_collision {
        [
            ("first", "First namespace description."),
            ("second", "Second namespace description."),
        ]
        .into_iter()
        .map(|(name, description)| {
            DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
                name: "shared".to_string(),
                description: description.to_string(),
                tools: vec![DynamicToolNamespaceTool::Function(
                    DynamicToolFunctionSpec {
                        name: name.to_string(),
                        description: format!("The {name} tool."),
                        input_schema: json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false,
                        }),
                        defer_loading: false,
                    },
                )],
            })
        })
        .collect()
    } else {
        vec![DynamicToolSpec::Function(DynamicToolFunctionSpec {
            name: "update_plan".to_string(),
            description: "Collides with the built-in planning tool.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            defer_loading: false,
        })]
    };
    let codex_core::NewThread {
        thread_id, thread, ..
    } = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;

    thread
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "use the planning tool".to_string(),
                text_elements: Vec::new(),
            }])
            .on_start(codex_core::TurnStartOptions {
                root_turn_id: Some("root-turn".into()),
                ..Default::default()
            }),
        )
        .await?;

    let EventMsg::Error(error) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::Error(_))).await
    else {
        unreachable!("event predicate guarantees an error");
    };
    let expected_collision = if namespace_collision {
        "duplicate tool: shared"
    } else {
        "duplicate tool: functions.update_plan"
    };
    assert_eq!(error.message, expected_collision);

    let EventMsg::TurnComplete(completed) =
        wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await
    else {
        unreachable!("event predicate guarantees turn completion");
    };
    assert_eq!(completed.error, Some(error));
    thread.flush_rollout().await?;
    let history = test
        .thread_store
        .load_latest_model_context(codex_thread_store::LoadThreadHistoryParams {
            thread_id,
            include_archived: false,
        })
        .await?;
    let attribution = history.items.iter().find_map(|item| match item {
        codex_history::RolloutItem::EventMsg(EventMsg::TurnStarted(event))
            if event.turn_id == completed.turn_id =>
        {
            event.root_turn_id.as_deref()
        }
        _ => None,
    });
    assert_eq!(attribution, Some("root-turn"));

    assert!(
        server
            .received_requests()
            .await
            .context("mock server should expose received requests")?
            .iter()
            .all(|request| request.url.path() != "/v1/responses"),
        "a colliding turn should fail before making a model request"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_tool_collisions_do_not_duplicate_unrelated_compaction_errors() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let error = json!({
        "error": {
            "message": "compaction request is invalid",
            "code": "invalid_request",
        },
    });
    let compact_mock =
        mount_response_once(&server, ResponseTemplate::new(400).set_body_json(&error)).await;
    let mut builder = test_codex().with_config(|config| {
        config.tool_registry.error_on_tool_collisions = true;
        config.model_auto_compact_token_limit = Some(0);
    });
    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "trigger compaction".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut errors = Vec::new();
    wait_for_event(&test.codex, |event| match event {
        EventMsg::Error(error) => {
            errors.push(error.message.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert_eq!(
        errors,
        vec![format!("Error running remote compact task: {error}")]
    );
    assert_eq!(compact_mock.requests().len(), 1);

    Ok(())
}

#[test_case::test_case(false; "default_off")]
#[test_case::test_case(true; "opted_in")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_turn_environments_gate_environment_backed_tools(
    stable_environment_tools: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(move |config| {
        if stable_environment_tools {
            config.features.enable(Feature::StableEnvironmentTools);
        }
        config.update_plan_enabled = true;
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_environments("which tools are available?", Some(vec![]))
        .await?;

    let tools = tool_names(&response_mock.single_request().body_json());
    assert!(
        tools.contains(&"update_plan".to_string()),
        "non-environment tool should remain available; got {tools:?}"
    );
    for environment_tool in ["exec_command", "write_stdin", "apply_patch", "view_image"] {
        assert_eq!(
            tools.contains(&environment_tool.to_string()),
            stable_environment_tools,
            "unexpected {environment_tool} availability for empty environments; got {tools:?}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_environment_selection_keeps_environment_backed_tools() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    test.submit_turn_with_environments(
        "which tools are available?",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let tools = tool_names(&response_mock.single_request().body_json());
    assert!(
        tools.contains(&"exec_command".to_string()),
        "environment tool should remain available with selected local environment; got {tools:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tool_unknown_returns_custom_output_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let call_id = "custom-unsupported";
    let tool_name = "unsupported_tool";

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call(call_id, tool_name, "\"payload\""),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("invoke custom tool").await?;

    let item = mock.single_request().custom_tool_call_output(call_id);
    let output = item
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let expected = format!("unsupported custom tool call: {tool_name}");
    assert_eq!(output, expected);
    assert!(
        item.pointer("/internal_chat_message_metadata_passthrough/executed_tool_calls")
            .is_none(),
        "attempted-tool metadata must be disabled by default",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaced_custom_tool_call_preserves_namespace_through_dispatch_and_replay() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    builder = builder.with_config(|config| {
        let _ = config.features.enable(Feature::ExecutedToolCallMetadata);
    });
    let test = builder.build(&server).await?;

    let call_id = "custom-namespaced";
    let namespace = "test_namespace::";
    let tool_name = "unsupported_tool";
    let input = "\"payload\"";

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call_with_namespace(call_id, namespace, tool_name, input),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("invoke namespaced custom tool").await?;

    let request = mock.single_request();
    let custom_tool_calls = request.inputs_of_type("custom_tool_call");
    let turn_id = custom_tool_calls
        .first()
        .and_then(|item| item.pointer("/internal_chat_message_metadata_passthrough/turn_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .expect("custom tool call should include turn metadata");
    let custom_tool_output = request.custom_tool_call_output(call_id);
    let output_create_time = custom_tool_output
        .pointer("/internal_chat_message_metadata_passthrough/create_time")
        .and_then(Value::as_f64)
        .expect("custom tool output should include a creation timestamp");
    assert_eq!(
        (
            strip_response_item_ids_from_json(Value::Array(custom_tool_calls)),
            strip_response_item_ids_from_json(custom_tool_output),
        ),
        (
            Value::Array(vec![json!({
                "type": "custom_tool_call",
                "call_id": call_id,
                "namespace": namespace,
                "name": tool_name,
                "input": input,
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": turn_id,
                },
            })]),
            json!({
                "type": "custom_tool_call_output",
                "call_id": call_id,
                "output": format!("unsupported custom tool call: {namespace}{tool_name}"),
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": turn_id,
                    "create_time": output_create_time,
                    "executed_tool_calls": [{
                        "name": format!("{namespace}__{tool_name}"),
                        "arguments": input,
                    }],
                    "tool_calls_complete": true,
                },
            }),
        )
    );
    let escaped_call_id = "custom-namespaced-escaped";
    let escaped_input = "\\".repeat(4_096);
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-3"),
            ev_custom_tool_call_with_namespace(
                escaped_call_id,
                namespace,
                tool_name,
                &escaped_input,
            ),
            ev_completed("resp-3"),
        ]),
    )
    .await;
    let escaped_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-2", "done"),
            ev_completed("resp-4"),
        ]),
    )
    .await;
    test.submit_turn("invoke namespaced custom tool with escaped arguments")
        .await?;
    let escaped_request = escaped_mock.single_request();
    assert_eq!(
        tool_call_metadata(escaped_request.custom_tool_call_output(call_id)),
        json!({
            "executed_tool_calls": [{
                "name": format!("{namespace}__{tool_name}"),
                "arguments": input,
            }],
            "tool_calls_complete": true,
        }),
    );
    let expected_escaped_calls = json!([{
        "name": format!("{namespace}__{tool_name}"),
        "arguments": escaped_input,
    }]);
    assert_eq!(
        tool_call_metadata(escaped_request.custom_tool_call_output(escaped_call_id)),
        json!({"executed_tool_calls": expected_escaped_calls, "tool_calls_complete": true}),
    );

    let direct_exec_call_id = "custom-direct-exec";
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-5"),
            ev_custom_tool_call(direct_exec_call_id, "exec", input),
            ev_completed("resp-5"),
        ]),
    )
    .await;
    let direct_exec_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-3", "done"),
            ev_completed("resp-6"),
        ]),
    )
    .await;

    test.submit_turn("invoke direct custom exec outside code mode")
        .await?;

    let direct_exec_request = direct_exec_mock.single_request();
    assert_eq!(
        tool_call_metadata(direct_exec_request.custom_tool_call_output(call_id)),
        tool_call_metadata(escaped_request.custom_tool_call_output(call_id)),
    );
    assert_eq!(
        tool_call_metadata(direct_exec_request.custom_tool_call_output(escaped_call_id)),
        tool_call_metadata(escaped_request.custom_tool_call_output(escaped_call_id)),
    );
    let direct_exec_output = direct_exec_request.custom_tool_call_output(direct_exec_call_id);
    assert_eq!(
        direct_exec_output["output"],
        json!("unsupported custom tool call: exec"),
    );
    assert_eq!(
        tool_call_metadata(direct_exec_output),
        json!({
            "executed_tool_calls": [{
                "name": "exec",
                "arguments": input,
            }],
            "tool_calls_complete": true,
        }),
    );

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum CommandToolAvailability {
    Default,
    ShellToolDisabled,
    ModelDisabled,
}

async fn collect_tools(availability: CommandToolAvailability) -> Result<Vec<String>> {
    let server = start_mock_server().await;

    let responses = vec![sse(vec![
        ev_response_created("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ])];
    let mock = mount_sse_sequence(&server, responses).await;

    let mut builder = match availability {
        CommandToolAvailability::Default => test_codex(),
        CommandToolAvailability::ShellToolDisabled => test_codex().with_config(|config| {
            config.features.disable(Feature::ShellTool);
        }),
        CommandToolAvailability::ModelDisabled => {
            test_codex().with_model_info_override("gpt-5.4", |model| {
                model.shell_type = ConfigShellToolType::Disabled;
            })
        }
    };
    let test = builder.build(&server).await?;

    test.submit_turn("list tools").await?;

    let first_body = mock.single_request().body_json();
    Ok(tool_names(&first_body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_spec_toggle_end_to_end() -> Result<()> {
    skip_if_no_network!(Ok(()));

    for availability in [
        CommandToolAvailability::ShellToolDisabled,
        CommandToolAvailability::ModelDisabled,
    ] {
        let tools = collect_tools(availability).await?;
        for command_tool in ["exec_command", "write_stdin"] {
            assert!(
                !tools.iter().any(|name| name == command_tool),
                "tools list should not include {command_tool} for {availability:?}: {tools:?}"
            );
        }
    }

    for availability in [CommandToolAvailability::Default] {
        let tools = collect_tools(availability).await?;
        for command_tool in ["exec_command", "write_stdin"] {
            assert!(
                tools.iter().any(|name| name == command_tool),
                "tools list should include {command_tool} for {availability:?}: {tools:?}"
            );
        }
    }

    Ok(())
}

use codex_core::TurnInputRequest;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_reasoning_summary_text_delta;
use core_test_support::responses::ev_reasoning_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use std::sync::Mutex;
use tracing::Level;
use tracing_test::traced_test;

use tracing_subscriber::fmt::format::FmtSpan;
use tracing_test::internal::MockWriter;

fn extract_log_field(line: &str, key: &str) -> Option<String> {
    let quoted_prefix = format!("{key}=\"");
    if let Some(start) = line.find(&quoted_prefix) {
        let value_start = start + quoted_prefix.len();
        if let Some(end_rel) = line[value_start..].find('"') {
            return Some(line[value_start..value_start + end_rel].to_string());
        }
    }

    let bare_prefix = format!("{key}=");
    for token in line.split_whitespace() {
        let trimmed = token.trim_end_matches(',');
        if let Some(value) = trimmed.strip_prefix(&bare_prefix) {
            return Some(value.to_string());
        }
    }

    None
}

fn assert_empty_mcp_tool_fields(line: &str) -> Result<(), String> {
    let mcp_server = extract_log_field(line, "mcp_server")
        .ok_or_else(|| "missing mcp_server field".to_string())?;
    if !mcp_server.is_empty() {
        return Err(format!("expected empty mcp_server, got {mcp_server}"));
    }

    let mcp_server_origin = extract_log_field(line, "mcp_server_origin")
        .ok_or_else(|| "missing mcp_server_origin field".to_string())?;
    if !mcp_server_origin.is_empty() {
        return Err(format!(
            "expected empty mcp_server_origin, got {mcp_server_origin}"
        ));
    }

    Ok(())
}

fn exec_command_call(call_id: &str, command: &str) -> serde_json::Value {
    let args = serde_json::json!({ "cmd": command }).to_string();
    ev_function_call(call_id, "exec_command", &args)
}

#[test]
fn extract_log_field_handles_empty_bare_values() {
    let line = "event.name=\"codex.tool_result\" mcp_server= mcp_server_origin=";
    assert_eq!(extract_log_field(line, "mcp_server"), Some(String::new()));
    assert_eq!(
        extract_log_field(line, "mcp_server_origin"),
        Some(String::new())
    );
}

#[test]
fn extract_log_field_does_not_confuse_similar_keys() {
    let line = "event.name=\"codex.tool_result\" mcp_server_origin=stdio";
    assert_eq!(extract_log_field(line, "mcp_server"), None);
    assert_eq!(
        extract_log_field(line, "mcp_server_origin"),
        Some("stdio".to_string())
    );
}

#[tokio::test]
#[traced_test]
async fn responses_api_emits_api_request_event() {
    let server = start_mock_server().await;

    let response_mock = mount_sse_once(&server, sse(vec![ev_completed("done")])).await;

    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.5")
        .with_config(|config| {
            config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
            config.model_reasoning_effort = Some(ReasoningEffort::High);
        })
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request_body = response_mock.single_request().body_json();
    assert_eq!(request_body["service_tier"].as_str(), Some("priority"));
    assert_eq!(request_body["reasoning"]["effort"].as_str(), Some("high"));

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| line.contains("codex.api_request"))
            .map(|_| Ok(()))
            .unwrap_or_else(|| Err("expected codex.api_request event".to_string()))
    });

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("event.kind=response.completed")
                    && line.contains("service_tier=\"priority\"")
                    && line.contains("model_reasoning_effort=\"high\"")
            })
            .map(|_| Ok(()))
            .unwrap_or_else(|| {
                Err("expected response.completed event with inference attributes".to_string())
            })
    });

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| line.contains("codex.conversation_starts"))
            .map(|_| Ok(()))
            .unwrap_or_else(|| Err("expected codex.conversation_starts event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_emits_tracing_for_output_item() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![ev_assistant_message("id1", "hi"), ev_completed("id2")]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex().build(&server).await.unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("event.kind=response.output_item.done")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing response.output_item.done event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_emits_failed_event_on_parse_error() {
    let server = start_mock_server().await;

    mount_sse_once(&server, "data: not-json\n\n".to_string()).await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("error.message")
                    && line.contains("expected ident at line 1 column 2")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_records_failed_event_when_stream_closes_without_completed() {
    let server = start_mock_server().await;

    mount_sse_once(&server, sse(vec![ev_assistant_message("id", "hi")])).await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("error.message")
                    && line.contains("stream closed before response.completed")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_failed_event_records_response_error_message() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "message": "boom",
                    "code": "bad"
                }
            }
        })]),
    )
    .await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "local shell done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("event.kind=response.failed")
                    && line.contains("error.message")
                    && line.contains("boom")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_failed_event_logs_parse_error() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": "not-an-object"
            }
        })]),
    )
    .await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "local shell done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event") && line.contains("event.kind=response.failed")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_failed_event_logs_missing_error() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![serde_json::json!({
            "type": "response.failed",
            "response": {}
        })]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event") && line.contains("event.kind=response.failed")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_failed_event_logs_response_completed_parse_error() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![serde_json::json!({
            "type": "response.completed",
            "response": {}
        })]),
    )
    .await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "local shell done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("event.kind=response.completed")
                    && line.contains("error.message")
                    && line.contains("failed to parse ResponseCompleted")
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing codex.sse_event".to_string()))
    });
}

#[tokio::test]
#[traced_test]
async fn process_sse_emits_completed_telemetry() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_reasoning_item_added("reasoning-1", &[]),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp1",
                    "usage": {
                        "input_tokens": 3,
                        "input_tokens_details": {
                            "cached_tokens": 1,
                            "cache_write_tokens": 2
                        },
                        "output_tokens": 5,
                        "output_tokens_details": { "reasoning_tokens": 2 },
                        "total_tokens": 9
                    }
                }
            }),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex().build(&server).await.unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        lines
            .iter()
            .find(|line| {
                line.contains("codex.sse_event")
                    && line.contains("event.kind=response.completed")
                    && line.contains("input_token_count=3")
                    && line.contains("output_token_count=5")
                    && line.contains("cached_token_count=1")
                    && line.contains("cache_write_token_count=2")
                    && line.contains("reasoning_token_count=2")
                    && line.contains("tool_token_count=9")
                    && extract_log_field(line, "ttft_ms")
                        .and_then(|ttft_ms| ttft_ms.parse::<i64>().ok())
                        .is_some()
            })
            .map(|_| Ok(()))
            .unwrap_or(Err("missing response.completed telemetry".to_string()))
    });
}

#[tokio::test(flavor = "current_thread")]
async fn turn_and_completed_response_spans_record_token_usage() {
    let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp1",
                "usage": {
                    "input_tokens": 3,
                    "input_tokens_details": {
                        "cached_tokens": 1,
                        "cache_write_tokens": 2
                    },
                    "output_tokens": 5,
                    "output_tokens_details": { "reasoning_tokens": 2 },
                    "total_tokens": 9
                }
            }
        })]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::High);
        })
        .build(&server)
        .await
        .unwrap();

    let TestCodex { codex, .. } = test;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

    assert!(
        logs.lines().any(|line| {
            line.contains("handle_responses{")
                && line.contains("otel.name=\"completed\"")
                && line.contains("codex.request.reasoning_effort=high")
                && line.contains("gen_ai.usage.input_tokens=3")
                && line.contains("gen_ai.usage.cache_read.input_tokens=1")
                && line.contains("gen_ai.usage.cache_write.input_tokens=2")
                && line.contains("gen_ai.usage.output_tokens=5")
                && line.contains("codex.usage.reasoning_output_tokens=2")
                && line.contains("codex.usage.total_tokens=9")
        }),
        "missing completed response span token usage\nlogs:\n{logs}"
    );
    assert!(
        logs.lines().any(|line| {
            line.contains("turn{otel.name=\"session_task.turn\"")
                && line.contains("codex.turn.reasoning_effort=high")
                && line.contains("codex.turn.token_usage.input_tokens=3")
                && line.contains("codex.turn.token_usage.cached_input_tokens=1")
                && line.contains("codex.turn.token_usage.cache_write_input_tokens=2")
                && line.contains("codex.turn.token_usage.non_cached_input_tokens=2")
                && line.contains("codex.turn.token_usage.output_tokens=5")
                && line.contains("codex.turn.token_usage.reasoning_output_tokens=2")
                && line.contains("codex.turn.token_usage.total_tokens=9")
        }),
        "missing regular turn span token usage\nlogs:\n{logs}"
    );
}

#[tokio::test]
async fn handle_responses_span_records_response_kind_and_tool_name() {
    let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_function_call("function-call", "nonexistent", "{\"value\":1}"),
            ev_completed("done"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "tool handled"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

    assert!(
        logs.lines().any(|line| {
            line.contains("handle_responses{")
                && line.contains("otel.name=\"function_call\"")
                && line.contains("tool_name=\"nonexistent\"")
                && line.contains("from=\"output_item_done\"")
        }),
        "missing handle_responses span with function call metadata\nlogs:\n{logs}"
    );
    assert!(
        logs.lines().any(|line| {
            line.contains("handle_responses{") && line.contains("otel.name=\"completed\"")
        }),
        "missing handle_responses span for completion\nlogs:\n{logs}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn record_responses_sets_span_fields_for_response_events() {
    let buffer: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = start_mock_server().await;

    let sse_body = sse(vec![
        ev_response_created("resp-1"),
        serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "call_id": "call-1",
                "name": "fn",
                "arguments": "{\"value\":1}"
            }
        }),
        ev_message_item_added("msg-added", "hi there"),
        ev_reasoning_item_added("reasoning-1", &["summary"]),
        ev_output_text_delta("delta"),
        ev_reasoning_summary_text_delta("summary-delta"),
        ev_reasoning_text_delta("raw-delta"),
        ev_function_call("call-1", "fn", "{\"key\":\"value\"}"),
        ev_assistant_message("msg-1", "agent"),
        ev_reasoning_item("reasoning-1", &["summary"], &[]),
        ev_completed("resp-1"),
    ]);

    mount_response_once(&server, sse_response(sse_body)).await;
    mount_response_once(
        &server,
        sse_response(sse(vec![
            ev_assistant_message("msg-2", "follow-up complete"),
            ev_completed("resp-2"),
        ])),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.model_reasoning_effort = Some(ReasoningEffort::High);
        })
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();

    let expected = [
        ("created", None::<&str>, None::<&str>),
        ("rate_limits", None, None),
        ("function_call", Some("output_item_added"), Some("fn")),
        ("message_from_assistant", Some("output_item_done"), None),
        ("reasoning", Some("output_item_done"), None),
        ("text_delta", None, None),
        ("reasoning_summary_delta", None, None),
        ("reasoning_content_delta", None, None),
        ("completed", None, None),
    ];

    for (name, from, tool_name) in expected {
        let otel_name = format!("otel.name=\"{name}\"");
        let from_field = from.map(|from| format!("from=\"{from}\""));
        let tool_name_field = tool_name.map(|tool_name| format!("tool_name=\"{tool_name}\""));

        assert!(
            logs.lines().any(|line| {
                line.contains("handle_responses{")
                    && line.contains(&otel_name)
                    && line.contains("codex.request.reasoning_effort=high")
                    && from_field
                        .as_ref()
                        .is_none_or(|from_field| line.contains(from_field))
                    && tool_name_field
                        .as_ref()
                        .is_none_or(|tool_name_field| line.contains(tool_name_field))
            }),
            "missing span fields for {name}\nlogs:\n{logs}"
        );
    }
}

#[tokio::test]
#[traced_test]
async fn handle_response_item_records_tool_result_for_custom_tool_call() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_custom_tool_call(
                "custom-tool-call",
                "unsupported_tool",
                "{\"key\":\"value\"}",
            ),
            ev_completed("done"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "local shell done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        let line = lines
            .iter()
            .find(|line| {
                line.contains("codex.tool_result") && line.contains("call_id=custom-tool-call")
            })
            .ok_or_else(|| "missing codex.tool_result event".to_string())?;

        if !line.contains("tool_name=unsupported_tool") {
            return Err("missing tool_name field".to_string());
        }
        if !line.contains("arguments={\"key\":\"value\"}") {
            return Err("missing arguments field".to_string());
        }
        if !line.contains("output=unsupported custom tool call: unsupported_tool") {
            return Err("missing output field".to_string());
        }
        if !line.contains("success=false") {
            return Err("missing success field".to_string());
        }
        assert_empty_mcp_tool_fields(line)?;

        Ok(())
    });
}

#[tokio::test]
#[traced_test]
async fn handle_response_item_records_tool_result_for_function_call() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_function_call("function-call", "nonexistent", "{\"value\":1}"),
            ev_completed("done"),
        ]),
    )
    .await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "local shell done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TokenCount(_))).await;

    logs_assert(|lines: &[&str]| {
        let line = lines
            .iter()
            .find(|line| {
                line.contains("codex.tool_result") && line.contains("call_id=function-call")
            })
            .ok_or_else(|| "missing codex.tool_result event".to_string())?;

        if !line.contains("tool_name=nonexistent") {
            return Err("missing tool_name field".to_string());
        }
        if !line.contains("arguments={\"value\":1}") {
            return Err("missing arguments field".to_string());
        }
        if !line.contains("output=unsupported call: nonexistent") {
            return Err("missing output field".to_string());
        }
        if !line.contains("success=false") {
            return Err("missing success field".to_string());
        }
        assert_empty_mcp_tool_fields(line)?;

        Ok(())
    });
}

#[tokio::test]
#[traced_test]
async fn handle_response_item_records_tool_result_for_exec_command_call() {
    let server = start_mock_server().await;

    mount_sse_once(
        &server,
        sse(vec![
            exec_command_call("shell-call", "echo shell"),
            ev_completed("done"),
        ]),
    )
    .await;

    mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "shell command done"),
            ev_completed("done"),
        ]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_config(|_config| {})
        .build(&server)
        .await
        .unwrap();

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    logs_assert(|lines: &[&str]| {
        let line = lines
            .iter()
            .find(|line| line.contains("codex.tool_result") && line.contains("call_id=shell-call"))
            .ok_or_else(|| "missing codex.tool_result event".to_string())?;

        if !line.contains("tool_name=exec_command") {
            return Err("missing tool_name field".to_string());
        }
        if !line.contains("arguments={\"cmd\":\"echo shell\"}") {
            return Err("missing arguments field".to_string());
        }
        let output_idx = line
            .find("output=")
            .ok_or_else(|| "missing output field".to_string())?;
        if line[output_idx + "output=".len()..].is_empty() {
            return Err("empty output field".to_string());
        }
        if !line.contains("success=false") {
            return Err("missing success field".to_string());
        }
        assert_empty_mcp_tool_fields(line)?;

        Ok(())
    });
}

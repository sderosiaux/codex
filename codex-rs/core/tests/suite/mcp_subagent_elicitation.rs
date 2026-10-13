//! MCP server elicitations: confirmations are accepted automatically, forms reach the client,
//! and pending forms are abandoned before shutdown.

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_protocol::elicitation::ElicitationAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

const SERVER: &str = r#"
import json
import sys

def send(message):
    print(json.dumps({"jsonrpc": "2.0", **message}), flush=True)

elicitation = json.loads(sys.argv[1])
pending = None
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "elicitation-test", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "request_input", "inputSchema": {"type": "object", "properties": {}},
                             "annotations": {"readOnlyHint": True}}]}
    elif method == "tools/call":
        pending = request["id"]
        send({"id": "input-request", "method": "elicitation/create", "params": elicitation})
        continue
    elif method is None and request.get("id") == "input-request":
        send({"id": pending, "result": {"content": [{"type": "text", "text": json.dumps(
            request.get("result", request.get("error")))}]}})
        continue
    elif method == "resources/list":
        result = {"resources": []}
    elif method == "resources/templates/list":
        result = {"resourceTemplates": []}
    elif "id" not in request:
        continue
    else:
        result = {}
    send({"id": request["id"], "result": result})
"#;

#[derive(Clone, Copy)]
pub(super) enum Caller {
    Root,
    Subagent,
}

#[derive(Clone, Copy)]
pub(super) enum RequestKind {
    BrowserAuth,
    FormInput,
    ShutdownFormInput,
    Confirmation,
}

#[test_case(Caller::Root, RequestKind::BrowserAuth; "root_browser_auth_confirmation_is_accepted")]
#[test_case(Caller::Root, RequestKind::ShutdownFormInput; "pending_root_form_is_abandoned_before_shutdown")]
#[test_case(Caller::Subagent, RequestKind::FormInput; "subagent_receives_form_input")]
#[test_case(Caller::Subagent, RequestKind::Confirmation; "subagent_confirmation_is_accepted")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_server_elicitations_reach_the_client_only_for_input(
    caller: Caller,
    request_kind: RequestKind,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_wine_exec!(Ok(()), "the MCP fixture requires a host Python interpreter");
    mcp_server_elicitation_scenario(caller, request_kind).await?;
    Ok(())
}

pub(super) async fn mcp_server_elicitation_scenario(
    caller: Caller,
    request_kind: RequestKind,
) -> Result<Vec<ResponsesRequest>> {
    let server = responses::start_mock_server().await;
    let test = test_codex().build_with_auto_env(&server).await?;
    let mut elicitation = json!({
        "mode": "form",
        "message": "Provide the requested input.",
        "requestedSchema": {"type": "object", "properties": {}}
    });
    match request_kind {
        RequestKind::BrowserAuth => {
            // Browser credentials travel through the browser broker, so the MCP schema is empty.
            elicitation["_meta"] = json!({
                "codex_approval_kind": "browser_auth",
                "browser_auth_challenge_id": "test_browser_auth_challenge_0123456789",
                "origin": "https://example.com",
                "reason": "Sign in to continue browsing.",
                "fields": [{"id": "password", "label": "Password", "type": "password", "required": true}]
            });
        }
        RequestKind::FormInput | RequestKind::ShutdownFormInput => {
            elicitation["requestedSchema"]["properties"] = json!({"answer": {"type": "string"}});
        }
        RequestKind::Confirmation => {
            elicitation["_meta"] = json!({
                "tool_name": "write_record",
                "tool_params": {"value": 42}
            });
        }
    }
    let mut config = test.config.clone();
    config.mcp_servers = serde_json::from_value(json!({
        "elicitation": {
            "command": if cfg!(windows) { "python" } else { "python3" },
            "args": ["-u", "-c", SERVER, elicitation.to_string()]
        }
    }))?;
    // Forms with fields need the user; confirmation-only forms carry nothing to collect.
    let expects_prompt = matches!(
        request_kind,
        RequestKind::FormInput | RequestKind::ShutdownFormInput
    );
    let session_source = if matches!(caller, Caller::Root) {
        SessionSource::Exec
    } else {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: test.session_configured.thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        })
    };
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(session_source),
            environments: Some(vec![test.executor_environment().request()]),
            ..StartThreadOptions::new(config)
        })
        .await?
        .thread;
    wait_for_mcp_server(&thread, "elicitation").await?;
    let tool_call = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_function_call_with_namespace(
                "input-call",
                "mcp__elicitation",
                "request_input",
                "{}",
            ),
            responses::ev_completed("tool-response"),
        ]),
    )
    .await;
    let follow_up = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("done-message", "done"),
            responses::ev_completed("done-response"),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Run the input tool.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let response_content = if matches!(request_kind, RequestKind::FormInput) {
        json!({"answer": "continue"})
    } else {
        json!({})
    };
    let mut prompts = 0;
    loop {
        let event = wait_for_event(&thread, |event| {
            matches!(
                event,
                EventMsg::ElicitationRequest(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        let EventMsg::ElicitationRequest(request) = event else {
            break;
        };
        assert!(
            expects_prompt,
            "automatically handled elicitations must not prompt the user"
        );
        assert_eq!(prompts, 0, "one tool call must not prompt twice");
        prompts += 1;
        assert!(
            follow_up.requests().is_empty(),
            "the tool must wait for input"
        );
        if matches!(request_kind, RequestKind::ShutdownFormInput) {
            thread.submit(Op::Shutdown).await?;
            let EventMsg::ElicitationAbandoned(abandoned) = wait_for_event(&thread, |event| {
                matches!(
                    event,
                    EventMsg::ElicitationAbandoned(_) | EventMsg::ShutdownComplete
                )
            })
            .await
            else {
                panic!("abandonment must precede shutdown completion");
            };
            assert_eq!(
                abandoned,
                codex_protocol::elicitation::ElicitationAbandonedEvent {
                    server_name: request.server_name,
                    id: request.id,
                }
            );
            assert!(matches!(
                wait_for_event(&thread, |event| {
                    matches!(
                        event,
                        EventMsg::ElicitationAbandoned(_) | EventMsg::ShutdownComplete
                    )
                })
                .await,
                EventMsg::ShutdownComplete
            ));
            assert!(follow_up.requests().is_empty());
            test.codex.shutdown_and_wait().await?;
            return Ok(tool_call.requests());
        }
        thread
            .submit(Op::ResolveElicitation {
                server_name: request.server_name,
                request_id: request.id,
                decision: ElicitationAction::Accept,
                content: Some(response_content.clone()),
                meta: None,
            })
            .await?;
    }
    assert_eq!(prompts, usize::from(expects_prompt));
    let output = follow_up
        .single_request()
        .function_call_output("input-call");
    let response: Value = serde_json::from_str(
        output["output"][1]["text"]
            .as_str()
            .expect("MCP response must reach the model"),
    )?;
    let expected = json!({"action": "accept", "content": response_content});
    assert_eq!(response, expected);
    let mut requests = tool_call.requests();
    requests.extend(follow_up.requests());
    thread.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(requests)
}

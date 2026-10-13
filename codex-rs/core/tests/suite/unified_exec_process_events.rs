use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnEnvironmentRequest;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::startup::STARTUP_TIMEOUT;
use core_test_support::startup::expect_startup;
use core_test_support::test_codex::test_codex;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_case::test_case;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const CALL_ID: &str = "pushed-remote-process-events";
const POLL_CALL_ID: &str = "poll-remote-process-events";
const COMPLETE_OUTPUT: &str = "pushed remote output\n";
const RECOVERED_OUTPUT: &str = "recovered missing output\n";
const RETAINED_OUTPUT: &str = "retained output\n";
const REPLAY_OUTPUT_EVENT_COUNT: u64 = 1024;
const REPLAY_RETAINED_OUTPUT_SEQ: u64 = 800;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(windows, allow(dead_code))]
enum PushedExecScenario {
    Complete,
    OutputAfterExit,
    OutputOpenAtDeadline,
    OutputOpenAtFinalPoll,
    OutputOpenWhenTerminated,
    CodexPath,
    CodexPathDefaultFeature,
    CodexPathDisabledFeature,
    CodexPathWithExplicitOverride,
    UnusableCodexPath,
    InterceptedPatch,
    ReplayGap,
}

#[derive(Debug)]
struct PushedExecServerResult {
    process_read_requests: usize,
    process_start: Value,
}

async fn read_exec_server_json(
    websocket: &mut WebSocketStream<TcpStream>,
    wait: Duration,
) -> Value {
    loop {
        match timeout(wait, websocket.next())
            .await
            .expect("websocket read should not time out")
            .expect("websocket should stay open")
            .expect("websocket frame should read")
        {
            Message::Text(text) => {
                return serde_json::from_str(text.as_ref()).expect("valid JSON-RPC message");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(bytes.as_ref()).expect("valid JSON-RPC message");
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected JSON-RPC message, got {other:?}"),
        }
    }
}

async fn send_exec_server_json(websocket: &mut WebSocketStream<TcpStream>, message: Value) {
    websocket
        .send(Message::Text(message.to_string().into()))
        .await
        .expect("exec-server message should send");
}

async fn accept_initialized_exec_server(listener: TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("connection");
    let mut websocket = accept_async(stream).await.expect("websocket handshake");

    let initialize = read_exec_server_json(&mut websocket, Duration::from_secs(/*secs*/ 5)).await;
    assert_eq!(initialize["method"], "initialize");
    send_exec_server_json(
        &mut websocket,
        json!({
            "id": initialize["id"],
            "result": { "sessionId": "test-session" }
        }),
    )
    .await;
    let initialized = read_exec_server_json(&mut websocket, Duration::from_secs(/*secs*/ 5)).await;
    assert_eq!(initialized["method"], "initialized");

    websocket
}

async fn send_environment_info(
    websocket: &mut WebSocketStream<TcpStream>,
    scenario: PushedExecScenario,
) {
    let info = read_exec_server_json(websocket, STARTUP_TIMEOUT).await;
    assert_eq!(info["method"], "environment/info");
    respond_environment_info(websocket, &info["id"], scenario).await;
}

async fn respond_environment_info(
    websocket: &mut WebSocketStream<TcpStream>,
    id: &Value,
    scenario: PushedExecScenario,
) {
    let mut response = json!({
        "id": id,
        "result": {
            "shell": { "name": "zsh", "path": "/bin/zsh" },
            "platformOs": null,
            "capabilities": {},
        }
    });
    if matches!(
        scenario,
        PushedExecScenario::CodexPath
            | PushedExecScenario::CodexPathDefaultFeature
            | PushedExecScenario::CodexPathDisabledFeature
            | PushedExecScenario::CodexPathWithExplicitOverride
    ) {
        response["result"]["prependPathDirs"] = json!([
            "file:///executor/codex-path",
            "file:///executor/extra-tools",
        ]);
    } else if matches!(scenario, PushedExecScenario::UnusableCodexPath) {
        response["result"]["prependPathDirs"] = json!(["file:///executor/codex%3Apath"]);
    }
    send_exec_server_json(websocket, response).await;
}

async fn serve_exec_with_pushed_events(
    listener: TcpListener,
    scenario: PushedExecScenario,
    process_started: Arc<AtomicBool>,
    output_ready: Arc<tokio::sync::Notify>,
) -> PushedExecServerResult {
    let intercepted_patch = matches!(scenario, PushedExecScenario::InterceptedPatch);
    let mut websocket = accept_initialized_exec_server(listener).await;
    send_environment_info(&mut websocket, scenario).await;

    let process_start = loop {
        // The runtime may still be finishing local setup before its first tool call.
        let request = read_exec_server_json(&mut websocket, STARTUP_TIMEOUT).await;
        match request["method"].as_str() {
            Some("process/start") => {
                process_started.store(true, Ordering::SeqCst);
                break request;
            }
            Some("environment/info") => {
                respond_environment_info(&mut websocket, &request["id"], scenario).await;
            }
            Some("fs/getMetadata") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "error": { "code": -32004, "message": "not found" }
                    }),
                )
                .await;
            }
            Some("fs/readFile") if intercepted_patch => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "error": { "code": -32004, "message": "not found" }
                    }),
                )
                .await;
            }
            Some("fs/writeFile") if intercepted_patch => {
                send_exec_server_json(&mut websocket, json!({ "id": request["id"], "result": {} }))
                    .await;
                return PushedExecServerResult {
                    process_read_requests: 0,
                    process_start: request,
                };
            }
            Some("fs/canonicalize") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "path": request["params"]["path"] }
                    }),
                )
                .await;
            }
            Some("fs/walk") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "entries": [], "errors": [], "truncated": false }
                    }),
                )
                .await;
            }
            method => panic!("unexpected exec-server request before process/start: {method:?}"),
        }
    };
    let process_id = process_start["params"]["processId"]
        .as_str()
        .expect("process/start should include processId")
        .to_string();

    let replay_output = |seq| -> &'static [u8] {
        match seq {
            1 => RECOVERED_OUTPUT.as_bytes(),
            REPLAY_RETAINED_OUTPUT_SEQ => RETAINED_OUTPUT.as_bytes(),
            _ => b"x",
        }
    };
    if matches!(scenario, PushedExecScenario::ReplayGap) {
        // The process replay log retains 256 events. This burst is much larger
        // than both that log and the JSON-RPC event queue, so the reader must
        // apply enough notifications to evict seq 1 before it can read the
        // start response. The total output stays well below the server's 1 MiB
        // retained-output limit, making the subsequent read genuinely able to
        // recover every missing chunk.
        for seq in 1..=REPLAY_OUTPUT_EVENT_COUNT {
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": seq,
                        "stream": "stdout",
                        "chunk": BASE64_STANDARD.encode(replay_output(seq)),
                    }
                }),
            )
            .await;
        }
        send_exec_server_json(
            &mut websocket,
            json!({
                "method": "process/exited",
                "params": {
                    "processId": &process_id,
                    "seq": REPLAY_OUTPUT_EVENT_COUNT + 1,
                    "exitCode": 0,
                }
            }),
        )
        .await;
    }

    send_exec_server_json(
        &mut websocket,
        json!({
            "id": process_start["id"],
            "result": { "processId": &process_id }
        }),
    )
    .await;

    let output_stays_open = matches!(
        scenario,
        PushedExecScenario::OutputOpenAtDeadline
            | PushedExecScenario::OutputOpenAtFinalPoll
            | PushedExecScenario::OutputOpenWhenTerminated
    );
    if output_stays_open {
        output_ready.notified().await;
    }
    let mut process_read_requests = 0;
    match scenario {
        PushedExecScenario::OutputAfterExit
        | PushedExecScenario::OutputOpenAtDeadline
        | PushedExecScenario::OutputOpenAtFinalPoll
        | PushedExecScenario::OutputOpenWhenTerminated => {
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": 1,
                        "stream": "stdout",
                        "chunk": BASE64_STANDARD.encode("pushed "),
                    }
                }),
            )
            .await;
            let exit = json!({
                "method": "process/exited",
                "params": {
                    "processId": &process_id,
                    "seq": 2,
                    "exitCode": 0,
                }
            });
            send_exec_server_json(&mut websocket, exit).await;
            // Model foreground output delayed in transit beyond the local post-exit grace.
            tokio::time::sleep(Duration::from_millis(250)).await;
            send_exec_server_json(
                &mut websocket,
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": 3,
                        "stream": "stdout",
                        "chunk": BASE64_STANDARD.encode("remote output\n"),
                    }
                }),
            )
            .await;
            if !output_stays_open {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "method": "process/closed",
                        "params": { "processId": &process_id, "seq": 4 }
                    }),
                )
                .await;
            }
        }
        PushedExecScenario::Complete
        | PushedExecScenario::CodexPath
        | PushedExecScenario::CodexPathDefaultFeature
        | PushedExecScenario::CodexPathDisabledFeature
        | PushedExecScenario::CodexPathWithExplicitOverride
        | PushedExecScenario::UnusableCodexPath => {
            let encoded_output = BASE64_STANDARD.encode(COMPLETE_OUTPUT);
            for message in [
                json!({
                    "method": "process/output",
                    "params": {
                        "processId": &process_id,
                        "seq": 1,
                        "stream": "stdout",
                        "chunk": encoded_output,
                    }
                }),
                json!({
                    "method": "process/exited",
                    "params": {
                        "processId": &process_id,
                        "seq": 2,
                        "exitCode": 0,
                    }
                }),
                json!({
                    "method": "process/closed",
                    "params": { "processId": &process_id, "seq": 3 }
                }),
            ] {
                send_exec_server_json(&mut websocket, message).await;
            }
        }
        PushedExecScenario::InterceptedPatch => {
            panic!("intercepted patches must write through the remote filesystem")
        }
        PushedExecScenario::ReplayGap => {}
    }

    loop {
        let wait = if output_stays_open {
            STARTUP_TIMEOUT
        } else {
            Duration::from_secs(/*secs*/ 5)
        };
        let request = read_exec_server_json(&mut websocket, wait).await;
        match request["method"].as_str() {
            Some("process/read") => {
                process_read_requests += 1;
                let result = match scenario {
                    PushedExecScenario::OutputAfterExit
                    | PushedExecScenario::OutputOpenAtDeadline
                    | PushedExecScenario::OutputOpenAtFinalPoll
                    | PushedExecScenario::OutputOpenWhenTerminated => {
                        panic!("unexpected read after the delayed-output sequence")
                    }
                    PushedExecScenario::Complete
                    | PushedExecScenario::CodexPath
                    | PushedExecScenario::CodexPathDefaultFeature
                    | PushedExecScenario::CodexPathDisabledFeature
                    | PushedExecScenario::CodexPathWithExplicitOverride
                    | PushedExecScenario::UnusableCodexPath => json!({
                        "chunks": [{
                            "seq": 1,
                            "stream": "stdout",
                            "chunk": BASE64_STANDARD.encode(COMPLETE_OUTPUT),
                        }],
                        "nextSeq": 4,
                        "exited": true,
                        "exitCode": 0,
                        "closed": true,
                        "failure": null,
                    }),
                    PushedExecScenario::InterceptedPatch => {
                        panic!("intercepted patches must not read a remote process")
                    }
                    PushedExecScenario::ReplayGap => {
                        let chunks = (1..=REPLAY_OUTPUT_EVENT_COUNT)
                            .map(|seq| {
                                json!({
                                    "seq": seq,
                                    "stream": "stdout",
                                    "chunk": BASE64_STANDARD.encode(replay_output(seq)),
                                })
                            })
                            .collect::<Vec<_>>();
                        json!({
                            "chunks": chunks,
                            "nextSeq": REPLAY_OUTPUT_EVENT_COUNT + 2,
                            "exited": true,
                            "exitCode": 0,
                            "closed": false,
                            "failure": null,
                        })
                    }
                };
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": result,
                    }),
                )
                .await;
                if matches!(scenario, PushedExecScenario::ReplayGap) && process_read_requests == 1 {
                    send_exec_server_json(
                        &mut websocket,
                        json!({
                            "method": "process/closed",
                            "params": {
                                "processId": &process_id,
                                "seq": REPLAY_OUTPUT_EVENT_COUNT + 2,
                            }
                        }),
                    )
                    .await;
                }
            }
            Some("process/terminate") => {
                send_exec_server_json(
                    &mut websocket,
                    json!({
                        "id": request["id"],
                        "result": { "running": false }
                    }),
                )
                .await;
                return PushedExecServerResult {
                    process_read_requests,
                    process_start,
                };
            }
            method => panic!("unexpected exec-server request: {method:?}"),
        }
    }
}

#[test_case(PushedExecScenario::Complete, false ; "complete_event_stream")]
#[test_case(PushedExecScenario::OutputAfterExit, false ; "output_after_exit")]
#[test_case(PushedExecScenario::OutputOpenAtDeadline, /*foreign_cwd*/ false ; "output_open_at_deadline")]
#[test_case(PushedExecScenario::OutputOpenAtFinalPoll, /*foreign_cwd*/ false ; "output_open_at_final_poll")]
#[test_case(PushedExecScenario::OutputOpenWhenTerminated, /*foreign_cwd*/ false ; "output_open_when_terminated")]
#[test_case(PushedExecScenario::CodexPath, false ; "codex_path_is_exported_before_nested_command")]
#[test_case(PushedExecScenario::CodexPathDefaultFeature, false ; "codex_path_is_not_exported_by_default")]
#[test_case(PushedExecScenario::CodexPathDisabledFeature, false ; "disabled_codex_path_preserves_original_login_command")]
#[test_case(PushedExecScenario::CodexPathWithExplicitOverride, false ; "explicit_path_override_preserves_original_login_command")]
#[test_case(PushedExecScenario::UnusableCodexPath, false ; "unusable_codex_path_preserves_requested_login_shell")]
#[test_case(PushedExecScenario::ReplayGap, false ; "truncated_event_replay")]
#[cfg_attr(not(windows), test_case(PushedExecScenario::Complete, true ; "foreign_windows_cwd"))]
#[cfg_attr(not(windows), test_case(PushedExecScenario::InterceptedPatch, true ; "foreign_windows_intercepted_patch_writes_remotely"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_consumes_pushed_remote_process_events(
    scenario: PushedExecScenario,
    foreign_cwd: bool,
) -> Result<()> {
    let finish_at_deadline = matches!(scenario, PushedExecScenario::OutputOpenAtDeadline);
    let finish_at_final_poll = matches!(scenario, PushedExecScenario::OutputOpenAtFinalPoll);
    let terminate_after_exit = matches!(scenario, PushedExecScenario::OutputOpenWhenTerminated);
    let explicit_path = "/user/configured/bin:/user/other/bin";
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let tool_call = match scenario {
        PushedExecScenario::CodexPathWithExplicitOverride | PushedExecScenario::UnusableCodexPath => ev_function_call(
            CALL_ID,
            "exec_command",
            &json!({
                "cmd": "sh -c 'command -v rg'",
                "login": true,
                "yield_time_ms": 1_000,
            })
            .to_string(),
        ),
        _ => ev_function_call(
            CALL_ID,
            "exec_command",
            &json!({
                "cmd": match scenario {
                    PushedExecScenario::InterceptedPatch => {
                        "apply_patch <<'PATCH'\n*** Begin Patch\n*** Add File: allowed.txt\n+allowed\n*** End Patch\nPATCH".to_string()
                    }
                    PushedExecScenario::CodexPath
                    | PushedExecScenario::CodexPathDefaultFeature
                    | PushedExecScenario::CodexPathDisabledFeature => "sh -c 'command -v rg'".to_string(),
                    _ => "pwd".to_string(),
                },
                "yield_time_ms": 1_000,
            })
            .to_string(),
        ),
    };
    let mut responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            tool_call,
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-2", "done"),
            ev_completed("resp-2"),
        ]),
    ];
    if finish_at_final_poll {
        responses.insert(
            /*index*/ 1,
            sse(vec![
                ev_response_created("resp-poll"),
                ev_function_call(
                    POLL_CALL_ID,
                    "write_stdin",
                    &json!({ "session_id": 1000, "chars": "", "yield_time_ms": 1_000 }).to_string(),
                ),
                ev_completed("resp-poll"),
            ]),
        );
    }
    let response_mock = mount_sse_sequence(&server, responses).await;
    let exec_server_url = format!("ws://{}", listener.local_addr()?);
    let process_started = Arc::new(AtomicBool::new(false));
    let output_ready = Arc::new(tokio::sync::Notify::new());
    let exec_server = tokio::spawn(serve_exec_with_pushed_events(
        listener,
        scenario,
        Arc::clone(&process_started),
        Arc::clone(&output_ready),
    ));
    let builder = test_codex().with_exec_server_url(exec_server_url);
    let mut builder = builder.with_config(move |config| {
        config.project_doc_max_bytes = 0;
        if matches!(scenario, PushedExecScenario::CodexPathDisabledFeature) {
            config.features.disable(Feature::LoginShellPackagePath);
        } else if !matches!(scenario, PushedExecScenario::CodexPathDefaultFeature) {
            config.features.enable(Feature::LoginShellPackagePath);
        }
        if matches!(scenario, PushedExecScenario::CodexPathWithExplicitOverride) {
            config
                .shell_environment_policy
                .r#set
                .insert("PATH".to_string(), explicit_path.to_string());
        }
    });
    let test = expect_startup(builder.build(&server)).await;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "run a one-shot remote command".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: foreign_cwd.then(|| {
                    let cwd = PathUri::parse("file:///C:/workspace").expect("valid executor cwd");
                    let workspace_roots = vec![
                        cwd.clone(),
                        PathUri::parse("file:///D:/other-workspace")
                            .expect("valid Windows workspace root"),
                    ];
                    TurnEnvironmentRequests::new(
                        test.config.cwd.clone(),
                        vec![TurnEnvironmentRequest {
                            environment_id: codex_exec_server::REMOTE_ENVIRONMENT_ID.to_string(),
                            cwd,
                            workspace_roots,
                            config: EnvironmentConfigState::FromThread,
                        }],
                    )
                }),
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    let mut saw_exec_command_begin = false;
    let mut exec_command_begin_command = None;
    let mut command_end = None;
    let mut stopped = false;
    let mut turn_finished = false;
    let wait_for_end = finish_at_deadline || finish_at_final_poll || terminate_after_exit;
    let event_timeout = if finish_at_deadline || finish_at_final_poll || terminate_after_exit {
        STARTUP_TIMEOUT
    } else {
        Duration::from_secs(/*secs*/ 5)
    };
    loop {
        let event = timeout(event_timeout, test.codex.next_event())
            .await
            .context("turn should complete")??
            .msg;
        match event {
            EventMsg::ExecCommandBegin(event) if event.call_id == CALL_ID => {
                saw_exec_command_begin = true;
                exec_command_begin_command = Some(event.command);
                if finish_at_deadline {
                    // Storage follows the streaming subscription. Wait for it before
                    // exiting so both deadline cleanup and cancellation see a live session.
                    timeout(STARTUP_TIMEOUT, async {
                        while test.codex.list_background_terminals().await.is_empty() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await?;
                    output_ready.notify_one();
                }
            }
            EventMsg::ExecCommandOutputDelta(event)
                if terminate_after_exit
                    && event.call_id == CALL_ID
                    && !stopped
                    && String::from_utf8_lossy(&event.chunk).contains("remote output") =>
            {
                // The exit precedes this output in the executor event stream.
                assert!(
                    test.codex
                        .terminate_background_terminal(/*process_id*/ 1000)
                        .await
                );
                stopped = true;
            }
            EventMsg::RawResponseItem(raw)
                if (finish_at_final_poll || terminate_after_exit)
                    && matches!(&raw.item, ResponseItem::FunctionCallOutput { call_id, .. } if call_id.as_deref() == Some(CALL_ID)) =>
            {
                // Polling and explicit termination must observe exit after the initial call yielded.
                output_ready.notify_one();
            }
            EventMsg::ExecCommandEnd(event) if event.call_id == CALL_ID => {
                command_end = Some(event);
            }
            EventMsg::TurnComplete(_) => turn_finished = true,
            _ => {}
        }
        if turn_finished && (!wait_for_end || command_end.is_some()) {
            break;
        }
    }
    if wait_for_end {
        assert_eq!(
            command_end.map(|end| (end.exit_code, end.aggregated_output)),
            Some((0, COMPLETE_OUTPUT.to_string()))
        );
        assert!(test.codex.list_background_terminals().await.is_empty());
    }
    if matches!(scenario, PushedExecScenario::InterceptedPatch) {
        let request = response_mock
            .last_request()
            .context("model should receive the intercepted patch result")?;
        let (output, success) = request
            .function_call_output_content_and_success(CALL_ID)
            .context("intercepted patch result should be model visible")?;
        assert_ne!(success, Some(false));
        let output = output.context("intercepted patch result should contain text")?;
        assert!(
            output.contains("Success. Updated the following files:"),
            "intercepted patch failed: {output}"
        );
        let exec_server_result = timeout(Duration::from_secs(5), exec_server)
            .await
            .context("fake exec-server should observe the intercepted patch write")??;
        let write_request = exec_server_result.process_start;
        assert_eq!(write_request["method"], "fs/writeFile");
        assert_eq!(
            write_request["params"]["path"],
            "file:///C:/workspace/allowed.txt"
        );
        assert_eq!(
            BASE64_STANDARD.decode(
                write_request["params"]["dataBase64"]
                    .as_str()
                    .expect("filesystem write should include encoded contents")
            )?,
            b"allowed\n"
        );
        return Ok(());
    }
    let exec_server_result = timeout(Duration::from_secs(5), exec_server)
        .await
        .context("fake exec-server should observe process cleanup")??;
    assert_eq!(
        exec_server_result.process_start["params"]["metadata"],
        json!({
            "threadId": test.session_configured.thread_id,
            "toolCallId": CALL_ID,
        }),
    );
    if terminate_after_exit {
        assert!(stopped);
        assert_eq!(exec_server_result.process_read_requests, 0);
        return Ok(());
    }
    if matches!(scenario, PushedExecScenario::CodexPath) {
        let argv = &exec_server_result.process_start["params"]["argv"];
        assert_eq!(argv[0], "/bin/zsh");
        assert_eq!(argv[1], "-lc");
        let script = argv[2]
            .as_str()
            .context("shell command should contain a script")?;
        assert!(script.contains("export PATH=/executor/codex-path"));
        assert!(script.contains("export PATH=/executor/extra-tools"));
        assert!(script.ends_with("; sh -c 'command -v rg'"));
        assert_eq!(script.lines().count(), 1);
        assert_eq!(
            exec_command_begin_command,
            Some(vec![
                "/bin/zsh".to_string(),
                "-lc".to_string(),
                "sh -c 'command -v rg'".to_string()
            ])
        );
    } else if matches!(
        scenario,
        PushedExecScenario::CodexPathDefaultFeature
            | PushedExecScenario::CodexPathDisabledFeature
            | PushedExecScenario::CodexPathWithExplicitOverride
            | PushedExecScenario::UnusableCodexPath
    ) {
        let params = &exec_server_result.process_start["params"];
        let expected = vec!["/bin/zsh", "-lc", "sh -c 'command -v rg'"];
        assert_eq!(params["argv"], json!(expected));
        assert_eq!(
            exec_command_begin_command,
            Some(expected.into_iter().map(str::to_string).collect())
        );
        if matches!(scenario, PushedExecScenario::CodexPathWithExplicitOverride) {
            assert_eq!(params["envPolicy"]["set"]["PATH"], explicit_path);
        }
    } else if matches!(scenario, PushedExecScenario::Complete) && !foreign_cwd {
        assert_eq!(
            exec_server_result.process_start["params"]["argv"],
            json!(["/bin/zsh", "-lc", "pwd"])
        );
    }
    if foreign_cwd {
        let params = &exec_server_result.process_start["params"];
        assert_eq!(params["cwd"], "file:///C:/workspace");
    }
    let request = response_mock
        .last_request()
        .context("model should receive the exec_command output")?;
    let output_call_id = if finish_at_final_poll {
        let (initial_output, _) = request
            .function_call_output_content_and_success(CALL_ID)
            .context("initial exec_command should yield a session")?;
        assert!(
            initial_output
                .context("initial exec_command output should contain text")?
                .contains("Process running with session ID 1000")
        );
        assert!(
            !test
                .codex
                .terminate_background_terminal(/*process_id*/ 1000)
                .await
        );
        POLL_CALL_ID
    } else {
        CALL_ID
    };
    let (output, success) = request
        .function_call_output_content_and_success(output_call_id)
        .context("exec_command output should be model visible")?;
    let output = output.context("exec_command output should contain text")?;
    let process_read_requests = exec_server_result.process_read_requests;
    match scenario {
        PushedExecScenario::OutputAfterExit
        | PushedExecScenario::OutputOpenAtDeadline
        | PushedExecScenario::OutputOpenAtFinalPoll
        | PushedExecScenario::OutputOpenWhenTerminated => {
            assert_ne!(success, Some(false));
            assert!(saw_exec_command_begin);
            assert!(output.contains("Process exited with code 0"));
            assert!(
                output.contains(COMPLETE_OUTPUT),
                "missing remote output tail: {output}"
            );
            assert_eq!(process_read_requests, 0, "unexpected compatibility read");
        }
        PushedExecScenario::Complete
        | PushedExecScenario::CodexPath
        | PushedExecScenario::CodexPathDefaultFeature
        | PushedExecScenario::CodexPathDisabledFeature
        | PushedExecScenario::CodexPathWithExplicitOverride
        | PushedExecScenario::UnusableCodexPath => {
            assert_ne!(success, Some(false));
            assert!(saw_exec_command_begin);
            assert!(output.contains("Process exited with code 0"));
            assert!(output.contains(COMPLETE_OUTPUT));
            assert_eq!(process_read_requests, 0, "unexpected compatibility read");
        }
        PushedExecScenario::InterceptedPatch => {
            unreachable!("intercepted patch returned early")
        }
        PushedExecScenario::ReplayGap => {
            assert_ne!(success, Some(false));
            assert!(saw_exec_command_begin);
            assert_eq!(output.matches(RECOVERED_OUTPUT).count(), 1);
            assert_eq!(output.matches(RETAINED_OUTPUT).count(), 1);
            assert_eq!(process_read_requests, 1, "expected replay recovery read");
        }
    }

    Ok(())
}

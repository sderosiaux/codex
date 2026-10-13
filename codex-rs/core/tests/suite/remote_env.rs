#[path = "remote_env_capability_roots_tests.rs"]
mod capability_roots;
#[path = "remote_env_required_skills_tests.rs"]
mod required_skills;
#[path = "remote_env_spawn_tests.rs"]
pub(super) mod spawn_tests;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_api::AuthProvider;
use codex_core::CodexThreadSettingsOverrides;
use codex_core::EnvironmentConfig;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_core::WaitForEnvironmentToolConfig;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::config::Config;
use codex_exec_server::CopyOptions;
use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::EnvironmentReadyInfo;
use codex_exec_server::ExecServerError;
use codex_exec_server::NoiseChannelPublicKey;
use codex_exec_server::NoiseRendezvousConnectBundle;
use codex_exec_server::NoiseRendezvousConnectProvider;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_exec_server::RemoteEnvironmentConfig;
use codex_exec_server::RemoveOptions;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ENVIRONMENTS_INSTRUCTIONS_OPEN_TAG;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnEnvironmentRequest;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::TurnSettingsUpdate;
use codex_protocol::protocol::TurnSettingsUpdateOutcome;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use core_test_support::TestTargetOs;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_target_windows;
use core_test_support::startup::expect_startup;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::environment_config_for_selection;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::test_env;
use core_test_support::test_docker_container_name;
use core_test_support::test_target_os;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use futures::SinkExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tempfile::TempDir;
use test_case::test_case;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const WAIT_FOR_ENVIRONMENT_TEST_TOOL_DESCRIPTION: &str = "Test wait tool description";
const WAIT_FOR_ENVIRONMENT_TEST_ENVIRONMENT_ID_DESCRIPTION: &str =
    "Test environment ID description";

struct WaitForEnvironmentTestExtension;

impl ThreadLifecycleContributor<Config> for WaitForEnvironmentTestExtension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.thread_store.insert(WaitForEnvironmentToolConfig {
                tool_description: WAIT_FOR_ENVIRONMENT_TEST_TOOL_DESCRIPTION.to_string(),
                environment_id_description: WAIT_FOR_ENVIRONMENT_TEST_ENVIRONMENT_ID_DESCRIPTION
                    .to_string(),
            });
        })
    }
}

#[derive(Default)]
struct ReadyCapabilityRootsTestExtension {
    observed_roots: Option<Arc<Mutex<Vec<Vec<SelectedCapabilityRoot>>>>>,
}

impl ContextContributor for ReadyCapabilityRootsTestExtension {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        if let Some(observed_roots) = &self.observed_roots {
            observed_roots
                .lock()
                .expect("observed capability roots should not be poisoned")
                .push(input.ready_selected_capability_roots.to_vec());
        }
        let root_ids = input
            .ready_selected_capability_roots
            .iter()
            .map(|root| root.id.clone())
            .collect::<Vec<_>>();
        Box::pin(async move {
            let body = root_ids.join(",");
            vec![WorldStateSectionContribution::new(
                "ready_capability_roots_test",
                move |_| {
                    (
                        Some(json!(root_ids)),
                        Some(RenderedWorldStateFragment::new(
                            "user",
                            ("<ready_capability_roots>", "</ready_capability_roots>"),
                            body.clone(),
                        )),
                    )
                },
            )]
        })
    }
}

fn test_codex_with_wait_for_environment() -> TestCodexBuilder {
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(WaitForEnvironmentTestExtension));
    test_codex().with_extensions(Arc::new(extensions.build()))
}

/// Starts a turn on the given environments without waiting for it to finish.
async fn start_turn_with_environments(
    test: &TestCodex,
    prompt: &str,
    environments: Vec<TurnEnvironmentSelection>,
) -> Result<()> {
    let turn_environment_selections = codex_protocol::protocol::TurnEnvironmentSelections::new(
        test.config.cwd.clone(),
        environments,
    );
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(turn_environment_selections.into_requests()),
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
    Ok(())
}

async fn unified_exec_test(server: &wiremock::MockServer) -> Result<TestCodex> {
    let mut builder = test_codex();
    builder.build_with_remote_and_local_env(server).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_can_connect_and_use_filesystem() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));

    let test_env = test_env().await?;
    let file_system = test_env.environment().get_filesystem();

    let file_path_uri = test_env.selection().cwd.join("remote-test-env-ok")?;
    let payload = b"remote-test-env-ok".to_vec();

    file_system
        .write_file(&file_path_uri, payload.clone(), Default::default())
        .await?;
    let actual = file_system
        .read_file(&file_path_uri, Default::default())
        .await?;
    assert_eq!(actual, payload);

    file_system
        .remove(
            &file_path_uri,
            RemoveOptions {
                recursive: false,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_exposes_target_shell_and_exec_guidance_to_model() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));

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
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("report remote environment").await?;

    let request = response_mock.single_request();
    let body = request.body_json();
    let exec_command = body["tools"]
        .as_array()
        .context("tools should be an array")?
        .iter()
        .find(|tool| tool["name"] == "exec_command")
        .context("exec_command should be available")?;
    let has_windows_guidance = exec_command["description"]
        .as_str()
        .is_some_and(|description| description.contains("Windows safety rules:"));
    assert_eq!(
        has_windows_guidance,
        matches!(test_target_os(), TestTargetOs::Windows)
    );
    let environment_context = request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.starts_with("<environment_context>"))
        .context("environment context should be model visible")?;
    // TODO(anp): Assert Wine-exec exposes a `C:\\...` cwd after model-visible paths preserve
    // target-native spelling instead of the Linux orchestrator's `/C:/...` representation.
    let expected_shell = match test_target_os() {
        TestTargetOs::Linux => "<shell>bash</shell>",
        TestTargetOs::Windows => "<shell>powershell</shell>",
        TestTargetOs::MacOs => unreachable!("remote test targets do not run macOS"),
    };
    assert_eq!(
        environment_context
            .lines()
            .find(|line| line.trim_start().starts_with("<shell>"))
            .map(str::trim),
        Some(expected_shell),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn step_world_state_gates_deferred_prompt_independently_of_host_config() -> Result<()> {
    for deferred_executor_enabled in [false, true] {
        for host_config_present in [false, true] {
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
            let builder = if host_config_present {
                test_codex_with_wait_for_environment()
            } else {
                test_codex()
            };
            let mut builder = builder.with_config(move |config| {
                if deferred_executor_enabled {
                    config.features.enable(Feature::DeferredExecutor);
                }
            });
            let test = builder.build(&server).await?;

            test.submit_turn("report the environment").await?;

            let request = response_mock.single_request();
            let user_context = request.message_input_texts("user");
            assert_eq!(
                user_context
                    .iter()
                    .filter(|text| text.contains("<environment_context>"))
                    .count(),
                1,
                "deferred executor enabled: {deferred_executor_enabled}; host config present: {host_config_present}",
            );
            assert_eq!(
                environment_instructions_occurrences(&request),
                usize::from(deferred_executor_enabled),
            );
            assert_eq!(
                tool_names(&request.body_json()).contains(&"wait_for_environment".to_string()),
                deferred_executor_enabled,
            );
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_update_does_not_retarget_active_turn_environment() -> Result<()> {
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "pause-turn",
                    "request_user_input",
                    &json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue after settings update?",
                            "options": [{
                                "label": "Yes (Recommended)",
                                "description": "Continue the test."
                            }, {
                                "label": "No",
                                "description": "Stop the test."
                            }]
                        }]
                    })
                    .to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "first turn done"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "second turn done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::DeferredExecutor);
        config.features.enable(Feature::DefaultModeRequestUserInput);
    });
    let test = builder.build(&server).await?;
    let initial_cwd = test.config.cwd.clone();
    let initial_environments = test.codex.environment_selections().await;
    let next_workspace = TempDir::new()?;
    let next_cwd = next_workspace.path().abs();
    let next_environments =
        TurnEnvironmentSelections::new(next_cwd.clone(), vec![local(next_cwd.clone())]);

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "pause before continuing".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;

    let preview = test
        .codex
        .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
            environments: Some(next_environments.clone().into_requests()),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        preview.environment_selections(),
        &next_environments.environments
    );
    assert_eq!(preview.cwd(), &next_cwd);
    assert_eq!(preview.workspace_roots, vec![next_cwd.clone()]);
    assert_eq!(
        test.codex.environment_selections().await,
        initial_environments
    );

    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            environments: Some(next_environments.clone().into_requests()),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        test.codex.environment_selections().await,
        next_environments.environments
    );
    let snapshot = test.codex.config_snapshot().await;
    assert_eq!(
        snapshot.environment_selections(),
        next_environments.environments
    );
    assert_eq!(snapshot.cwd(), &next_cwd);
    assert_eq!(snapshot.workspace_roots, vec![next_cwd.clone()]);
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "continue".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes (Recommended)".to_string()],
                    },
                )]),
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("start the next turn").await?;

    let request_texts = response_mock
        .requests()
        .iter()
        .map(|request| request.message_input_texts("user").join("\n"))
        .collect::<Vec<_>>();
    let initial_cwd = format!("<cwd>{}</cwd>", initial_cwd.as_path().display());
    let next_cwd = format!("<cwd>{}</cwd>", next_cwd.as_path().display());
    assert_eq!(
        request_texts
            .iter()
            .map(|text| text.contains(&next_cwd))
            .collect::<Vec<_>>(),
        vec![false, false, true]
    );
    assert!(request_texts[0].contains(&initial_cwd));
    assert!(request_texts[1].contains(&initial_cwd));

    Ok(())
}

#[test_case(false; "configuration supplied with the selection")]
#[test_case(true; "configuration arrives after the selection")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_environment_update_waits_for_a_configured_executor_to_connect(
    configuration_arrives_later: bool,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let exec_server_url = format!("ws://{}", listener.local_addr()?);
    let (attach, connection) = tokio::sync::oneshot::channel();
    let (shutdown, stop) = tokio::sync::oneshot::channel();
    let executor = tokio::spawn(serve_environment_with_agents_md(
        listener, "", connection, stop,
    ));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call(
                    "pause-for-switch",
                    "request_user_input",
                    &json!({"questions": [{
                        "id": "continue", "header": "Continue", "question": "Continue?",
                        "options": [
                            {"label": "Yes", "description": "Continue the test."},
                            {"label": "No", "description": "Stop the test."}
                        ]
                    }]})
                    .to_string(),
                ),
                ev_completed("first"),
            ]),
            sse(vec![ev_completed("second")]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.project_doc_max_bytes = 0;
        config.features.disable(Feature::DeferredExecutor);
        config.features.enable(Feature::DefaultModeRequestUserInput);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let remote_environment_id = "connecting";
    test.thread_manager
        .environment_manager()
        .upsert_environment(
            remote_environment_id.to_string(),
            exec_server_url,
            /*connect_timeout*/ None,
        )?;
    let remote_cwd = test
        .executor_environment()
        .selection()
        .cwd
        .join("remote-workspace")?;
    let mut remote_selection = TurnEnvironmentSelection {
        selected_capability_roots: Default::default(),
        environment_id: remote_environment_id.to_string(),
        cwd: remote_cwd.clone(),
        workspace_roots: vec![remote_cwd.clone()],
        config: EnvironmentConfigState::Pending,
    };
    let owner_config = environment_config_for_selection(&test.config, &remote_selection);
    if !configuration_arrives_later {
        remote_selection.config = EnvironmentConfigState::Ready(owner_config.clone());
    }
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "pause before switching".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;

    let (reply, outcome) = tokio::sync::oneshot::channel();
    test.codex
        .submit(Op::TurnSettings {
            turn_id: request.turn_id.clone(),
            update: TurnSettingsUpdate {
                environments: Some(vec![remote_selection.clone().into_request()]),
                ..Default::default()
            },
            reply,
        })
        .await?;
    assert_eq!(outcome.await?, TurnSettingsUpdateOutcome::Applied);
    if configuration_arrives_later {
        test.codex
            .environment_ready(&remote_selection, owner_config)
            .await?;
    }
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "continue".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes".to_string()],
                    },
                )]),
            },
        })
        .await?;

    assert!(
        timeout(
            Duration::from_secs(/*secs*/ 1),
            wait_for_response_request_count(&responses, /*expected_count*/ 2)
        )
        .await
        .is_err(),
        "the follow-up must not run before the configured executor connects"
    );
    attach.send(()).expect("release executor startup");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = responses.requests();
    let context = requests[1]
        .message_input_texts("user")
        .into_iter()
        .rfind(|text| text.contains("<environment_context>"))
        .context("remote environment should be model visible")?;
    assert!(context.contains(&format!(
        "<cwd>{}</cwd>",
        remote_cwd.inferred_native_path_string()
    )));
    assert!(!context.contains("<status>starting</status>"));

    let _ = shutdown.send(());
    executor.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_promotes_primary_environment_when_startup_completes() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let apps = core_test_support::apps_test_server::AppsTestServer::mount(&server).await?;
    let mcp_url = format!("{}/api/codex/ps/mcp", apps.chatgpt_base_url);
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("warmup"),
                ev_assistant_message("warmup-message", "ready"),
                ev_completed("warmup"),
            ]),
            sse(vec![
                ev_response_created("before-promotion"),
                ev_function_call(
                    "pause-for-environment",
                    "request_user_input",
                    &json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue after the environment starts?",
                            "options": [{
                                "label": "Yes (Recommended)",
                                "description": "Continue the test."
                            }, {
                                "label": "No",
                                "description": "Stop the test."
                            }]
                        }]
                    })
                    .to_string(),
                ),
                ev_completed("before-promotion"),
            ]),
            sse(vec![
                ev_response_created("after-promotion"),
                ev_assistant_message("after-promotion-message", "done"),
                ev_completed("after-promotion"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = false)
        .with_config(move |config| {
            config.mcp_servers = HashMap::from([(
                "deferred".to_string(),
                serde_json::from_value(json!({
                    "url": mcp_url,
                    "environment_id": REMOTE_ENVIRONMENT_ID,
                    "startup_timeout_sec": 60,
                }))
                .expect("MCP server config"),
            )]);
            config.project_doc_max_bytes = 0;
            config.features.enable(Feature::DeferredExecutor);
            config.features.enable(Feature::DefaultModeRequestUserInput);
        });
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let local_selection = local(test.config.cwd.clone());
    let remote_selection = TurnEnvironmentSelection {
        selected_capability_roots: Default::default(),
        environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(&test.config.cwd),
        workspace_roots: vec![PathUri::from_abs_path(&test.config.cwd)],
        config: EnvironmentConfigState::FromThread,
    };

    test.submit_turn_with_environments(
        "warm the local environment",
        Some(vec![local_selection.clone(), remote_selection.clone()]),
    )
    .await?;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "wait for the primary environment".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![
                        remote_selection.into_request(),
                        local_selection.into_request(),
                    ],
                )),
                ..Default::default()
            }),
        )
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;

    let requests = response_mock.requests();
    let initial_context = requests[1]
        .message_input_texts("user")
        .into_iter()
        .rfind(|text| text.contains("<environment_context>"))
        .context("starting environment context")?;
    assert!(initial_context.contains("<environment id=\"local\" primary=\"true\">"));
    assert!(initial_context.contains("<environment id=\"remote\" primary=\"false\">"));
    assert!(initial_context.contains("<status>starting</status>"));
    assert!(
        requests[1]
            .tool_by_name("mcp__deferred", "calendar_list_events")
            .is_none()
    );

    let mut websocket = accept_initialized_exec_server(listener).await;
    // Forward MCP HTTP through the fake executor while keeping startup under test control.
    let http_client =
        codex_exec_server::Environment::create_for_tests(/*exec_server_url*/ None)?
            .get_http_client();
    let executor = tokio::spawn(async move {
        loop {
            let request = read_exec_server_json(&mut websocket).await;
            let Some(id) = request.get("id") else {
                continue;
            };
            let mut messages = Vec::new();
            if request["method"] == "http/request" {
                let params = serde_json::from_value(request["params"].clone()).unwrap();
                let mut response =
                    serde_json::to_value(http_client.http_request(params).await.unwrap()).unwrap();
                if request["params"]["streamResponse"] == true {
                    let body = response["bodyBase64"].take();
                    response["bodyBase64"] = json!("");
                    messages.push(json!({
                        "method": "http/request/bodyDelta",
                        "params": { "requestId": request["params"]["requestId"], "seq": 1,
                            "deltaBase64": body, "done": true }
                    }));
                }
                messages.insert(0, json!({ "id": id, "result": response }));
            } else if request["method"] == "environment/info" {
                messages.push(json!({ "id": id,
                    "result": { "shell": { "name": "zsh", "path": "/bin/zsh" } }
                }));
            } else {
                messages.push(
                    json!({ "id": id, "error": { "code": -32601, "message": "unsupported" } }),
                );
            }
            for message in messages {
                websocket
                    .send(Message::Text(message.to_string().into()))
                    .await
                    .unwrap();
            }
        }
    });
    core_test_support::wait_for_mcp_server(&test.codex, "deferred").await?;
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "continue".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes (Recommended)".to_string()],
                    },
                )]),
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response_mock.requests();
    assert!(
        requests[2]
            .tool_by_name("mcp__deferred", "calendar_list_events")
            .is_some()
    );
    let updated_context = requests[2]
        .message_input_texts("user")
        .into_iter()
        .rfind(|text| text.contains("<environment_context>"))
        .context("updated primary environment context")?;
    assert!(updated_context.contains("<environment id=\"local\" primary=\"false\">"));
    assert!(updated_context.contains("<environment id=\"remote\" primary=\"true\">"));
    assert!(updated_context.contains("<shell>zsh</shell>"));

    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let rollout = fs::read_to_string(test.codex.rollout_path().context("rollout path")?)?;
    let world_state_patch = rollout
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::WorldState(item) if !item.full => Some(Value::Object(item.state)),
            _ => None,
        })
        .find(|patch| {
            patch.pointer("/environments/environments/remote/is_primary") == Some(&json!(true))
        })
        .context("primary environment World State patch")?;
    assert_eq!(
        world_state_patch.pointer("/environments/environments/local/is_primary"),
        Some(&Value::Null)
    );

    executor.abort();
    let _ = executor.await;
    Ok(())
}

async fn read_exec_server_json(websocket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        match timeout(Duration::from_secs(5), websocket.next())
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

async fn accept_initialized_exec_server(listener: TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("connection");
    let mut websocket = accept_async(stream).await.expect("websocket handshake");

    let initialize = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialize["method"], "initialize");
    websocket
        .send(Message::Text(
            json!({
                "id": initialize["id"],
                "result": { "sessionId": "test-session" }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("initialize response");
    let initialized = read_exec_server_json(&mut websocket).await;
    assert_eq!(initialized["method"], "initialized");

    websocket
}

async fn send_environment_info(websocket: &mut WebSocketStream<TcpStream>) {
    let info = read_exec_server_json(websocket).await;
    assert_eq!(info["method"], "environment/info");
    websocket
        .send(Message::Text(
            json!({
                "id": info["id"],
                "result": { "shell": { "name": "zsh", "path": "/bin/zsh" } }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("environment info response");
}

async fn serve_environment_info(listener: TcpListener) {
    let mut websocket = accept_initialized_exec_server(listener).await;
    send_environment_info(&mut websocket).await;
}

async fn serve_environment_with_agents_md(
    listener: TcpListener,
    contents: &str,
    attach: tokio::sync::oneshot::Receiver<()>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> usize {
    serve_environment_with_instruction_files(
        listener, contents, /*skill*/ None, attach, shutdown,
    )
    .await
}

async fn serve_environment_with_instruction_files(
    listener: TcpListener,
    contents: &str,
    skill: Option<&str>,
    attach: tokio::sync::oneshot::Receiver<()>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> usize {
    let mut websocket = accept_initialized_exec_server(listener).await;
    attach.await.expect("attach signal");
    send_environment_info(&mut websocket).await;

    let mut agents_md_reads = 0;
    loop {
        let request = tokio::select! {
            request = read_exec_server_json(&mut websocket) => request,
            _ = &mut shutdown => return agents_md_reads,
        };
        let is_agents_md = request["params"]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/AGENTS.md"));
        let is_skill_root = skill.is_some()
            && request["params"]["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("/.agents/skills"));
        let response = match request["method"].as_str() {
            Some("environment/info") => json!({
                "id": request["id"],
                "result": { "shell": { "name": "zsh", "path": "/bin/zsh" } }
            }),
            Some("fs/canonicalize") => json!({
                "id": request["id"],
                "result": { "path": request["params"]["path"] }
            }),
            Some("fs/walk") => {
                let root = request["params"]["path"].as_str().expect("walk root");
                let entries = if is_skill_root {
                    vec![json!({
                        "path": format!("{root}/guardian-fixture-skill/SKILL.md"),
                        "kind": "file",
                    })]
                } else {
                    Vec::new()
                };
                json!({
                    "id": request["id"],
                    "result": { "entries": entries, "errors": [], "truncated": false }
                })
            }
            Some("fs/readFile")
                if skill.is_some()
                    && request["params"]["path"]
                        .as_str()
                        .is_some_and(|path| path.ends_with("/guardian-fixture-skill/SKILL.md")) =>
            {
                json!({
                    "id": request["id"],
                    "result": { "dataBase64": BASE64_STANDARD.encode(skill.expect("skill contents")) }
                })
            }
            Some("fs/getMetadata") if is_agents_md || is_skill_root => {
                json!({
                    "id": request["id"],
                    "result": {
                        "isDirectory": is_skill_root,
                        "isFile": is_agents_md,
                        "isSymlink": false,
                        "size": contents.len(),
                        "createdAtMs": 0,
                        "modifiedAtMs": 0,
                    }
                })
            }
            Some("fs/getMetadata") => json!({
                "id": request["id"],
                "error": { "code": -32004, "message": "not found" }
            }),
            Some("fs/readFile") if is_agents_md => {
                agents_md_reads += 1;
                json!({
                    "id": request["id"],
                    "result": { "dataBase64": BASE64_STANDARD.encode(contents) }
                })
            }
            method => panic!("unexpected exec-server request: {method:?}"),
        };
        websocket
            .send(Message::Text(response.to_string().into()))
            .await
            .expect("filesystem response");
    }
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

#[derive(Default)]
struct FailingNoiseConnectProvider {
    calls: AtomicUsize,
}

impl NoiseRendezvousConnectProvider for FailingNoiseConnectProvider {
    fn connect_bundle(
        &self,
        _: NoiseChannelPublicKey,
    ) -> BoxFuture<'_, std::result::Result<NoiseRendezvousConnectBundle, ExecServerError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async {
            Err(ExecServerError::Protocol(
                "test Noise connection failed".to_string(),
            ))
        })
    }
}

struct OfflineThenReadyNoiseConnectProvider {
    websocket_url: String,
    executor_public_key: NoiseChannelPublicKey,
    calls: AtomicUsize,
}

impl NoiseRendezvousConnectProvider for OfflineThenReadyNoiseConnectProvider {
    fn connect_bundle(
        &self,
        _: NoiseChannelPublicKey,
    ) -> BoxFuture<'_, std::result::Result<NoiseRendezvousConnectBundle, ExecServerError>> {
        if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            return Box::pin(async {
                Err(ExecServerError::EnvironmentRegistryHttp {
                    status: http::StatusCode::CONFLICT,
                    code: Some("environment_offline".to_string()),
                    message: "test environment is offline".to_string(),
                })
            });
        }
        let bundle = NoiseRendezvousConnectBundle {
            websocket_url: self.websocket_url.clone(),
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            executor_registration_id: "ready-first-registration".to_string(),
            executor_public_key: self.executor_public_key.clone(),
            harness_key_authorization: "ready-first-authorization".to_string(),
        };
        Box::pin(async move { Ok(bundle) })
    }
}

struct NoopRegistryAuthProvider;

impl AuthProvider for NoopRegistryAuthProvider {
    fn add_auth_headers(&self, _: &mut HeaderMap) {}
}

async fn wait_for_response_request_count(response_mock: &ResponseMock, expected_count: usize) {
    timeout(Duration::from_secs(5), async {
        while response_mock.requests().len() < expected_count {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for Responses API request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_executor_keeps_ready_capability_roots_scoped_to_each_attachment() -> Result<()> {
    let server = start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.prompt_contributor(Arc::new(ReadyCapabilityRootsTestExtension::default()));
    let mut builder = test_codex().with_extensions(Arc::new(extensions.build()));
    let test = builder.build_with_auto_env(&server).await?;
    let selection = test
        .codex
        .environment_selections()
        .await
        .into_iter()
        .next()
        .context("thread should select its executor environment")?;
    let root = |id: &str| SelectedCapabilityRoot {
        id: id.to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: selection.environment_id.clone(),
            path: selection.cwd.clone(),
        },
    };
    for config in [
        EnvironmentConfigState::Pending,
        EnvironmentConfigState::Ready(EnvironmentConfig {
            allow_login_shell: false,
            workspace_roots: selection.workspace_roots.clone(),
            shell_environment_policy: Default::default(),
            selected_capability_roots: vec![root("duplicate"), root("duplicate")],
        }),
    ] {
        let should_succeed = matches!(config, EnvironmentConfigState::Pending);
        let selection_override = TurnEnvironmentSelection {
            config,
            ..selection.clone()
        };
        let preview = test
            .codex
            .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![selection_override.into_request()],
                )),
                ..Default::default()
            })
            .await;
        assert_eq!(preview.is_ok(), should_succeed);
        assert_eq!(
            test.codex.environment_selections().await,
            vec![selection.clone()]
        );
    }

    let mut second_thread_init = ExtensionDataInit::new();
    second_thread_init.insert(vec![root("startup-root")]);
    let second = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![TurnEnvironmentRequest {
                config: EnvironmentConfigState::Ready(EnvironmentConfig {
                    allow_login_shell: true,
                    workspace_roots: selection.workspace_roots.clone(),
                    shell_environment_policy: Default::default(),
                    selected_capability_roots: vec![root("startup-root"), root("second-root")],
                }),
                ..selection.clone().into_request()
            }]),
            thread_extension_init: second_thread_init,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;

    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                test.config.cwd.clone(),
                vec![TurnEnvironmentRequest {
                    config: EnvironmentConfigState::Ready(EnvironmentConfig {
                        allow_login_shell: false,
                        workspace_roots: selection.workspace_roots.clone(),
                        shell_environment_policy: Default::default(),
                        selected_capability_roots: vec![root("first-root")],
                    }),
                    ..selection.clone().into_request()
                }],
            )),
            ..Default::default()
        },
    )
    .await?;

    assert_eq!(
        second
            .thread
            .inspect_selected_capability_roots()
            .ready_roots,
        vec![root("startup-root"), root("second-root")]
    );

    let response_mock = mount_sse_sequence(
        &server,
        ["first", "second", "first-updated", "second-again"]
            .into_iter()
            .map(|response_id| {
                sse(vec![
                    ev_response_created(response_id),
                    ev_completed(response_id),
                ])
            })
            .collect(),
    )
    .await;

    for (index, (thread, prompt)) in [
        (&test.codex, "first"),
        (&second.thread, "second"),
        (&test.codex, "first-updated"),
        (&second.thread, "second-again"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]);
        if index == 2 {
            request = request.with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![TurnEnvironmentRequest {
                        config: EnvironmentConfigState::Ready(EnvironmentConfig {
                            allow_login_shell: false,
                            workspace_roots: selection.workspace_roots.clone(),
                            shell_environment_policy: Default::default(),
                            selected_capability_roots: vec![root("first-updated-root")],
                        }),
                        ..selection.clone().into_request()
                    }],
                )),
                ..Default::default()
            });
        }

        thread.start_or_steer_turn(request).await?;
        wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }

    assert_eq!(
        test.codex.inspect_selected_capability_roots().ready_roots,
        vec![root("first-updated-root")]
    );
    let requests = response_mock.requests();
    let root_fragments = requests
        .iter()
        .map(|request| {
            request
                .message_input_texts("user")
                .into_iter()
                .rfind(|text| text.contains("<ready_capability_roots>"))
                .context("ready capability roots should be model-visible")
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        root_fragments,
        vec![
            "<ready_capability_roots>first-root</ready_capability_roots>",
            "<ready_capability_roots>startup-root,second-root</ready_capability_roots>",
            "<ready_capability_roots>first-updated-root</ready_capability_roots>",
            "<ready_capability_roots>startup-root,second-root</ready_capability_roots>",
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_attachment_installs_configuration_before_waiting_turn_resumes() -> Result<()> {
    const WAIT_CALL_ID: &str = "wait-for-owner-configuration";
    const EXEC_CALL_ID: &str = "exec-before-owner-configuration";
    const STDIN_CALL_ID: &str = "stdin-without-environment";
    const LOGIN_CALL_ID: &str = "login-disabled-by-owner-configuration";
    const WAIT_MESSAGE: &str = "No usable execution environment is available. Wait for an environment to become available before using this tool.";

    let server = start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(WaitForEnvironmentTestExtension));
    extensions.prompt_contributor(Arc::new(ReadyCapabilityRootsTestExtension::default()));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::StableEnvironmentTools);
            config.features.enable(Feature::DeferredExecutor);
        });
    let test = builder.build_with_auto_env(&server).await?;
    let selection = test
        .codex
        .environment_selections()
        .await
        .into_iter()
        .next()
        .context("thread should select its executor environment")?;
    let pending_selection = TurnEnvironmentSelection {
        config: EnvironmentConfigState::Pending,
        ..selection.clone()
    };
    let owner_workspace_root = selection.cwd.join("owner-workspace")?;
    let root = |id: &str| SelectedCapabilityRoot {
        id: id.to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: selection.environment_id.clone(),
            path: selection.cwd.clone(),
        },
    };
    let owner_config = |id: &str, allow_login_shell: bool| EnvironmentConfig {
        allow_login_shell,
        workspace_roots: vec![selection.cwd.clone(), owner_workspace_root.clone()],
        shell_environment_policy: Default::default(),
        selected_capability_roots: vec![root(id)],
    };
    let start_pending_thread = || {
        test.thread_manager.start_thread(StartThreadOptions {
            environments: Some(vec![pending_selection.clone().into_request()]),
            ..StartThreadOptions::new(test.config.clone())
        })
    };
    let waiting = timeout(Duration::from_secs(5), start_pending_thread())
        .await
        .context("pending thread startup should not block")??;
    let requested_workspace_roots = waiting.thread.config_snapshot().await.workspace_roots;
    let independent = start_pending_thread().await?;
    let failed = start_pending_thread().await?;
    let empty = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;

    submit_thread_settings(
        &waiting.thread,
        ThreadSettingsOverrides {
            ..Default::default()
        },
    )
    .await?;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call(EXEC_CALL_ID, "exec_command", r#"{"cmd":"pwd"}"#),
                ev_function_call(STDIN_CALL_ID, "write_stdin", r#"{"session_id":1}"#),
                ev_completed("no-environment-exec"),
            ]),
            sse(vec![
                ev_assistant_message("no-environment-message", "waiting for an environment"),
                ev_completed("no-environment-done"),
            ]),
            sse(vec![
                ev_response_created("pending-configuration-exec"),
                ev_function_call(
                    EXEC_CALL_ID,
                    "exec_command",
                    &json!({ "cmd": "pwd" }).to_string(),
                ),
                ev_function_call(STDIN_CALL_ID, "write_stdin", r#"{"session_id":1}"#),
                ev_completed("pending-configuration-exec"),
            ]),
            sse(vec![
                ev_response_created("pending-configuration-wait"),
                ev_function_call(
                    WAIT_CALL_ID,
                    "wait_for_environment",
                    &json!({ "environment_id": selection.environment_id }).to_string(),
                ),
                ev_completed("pending-configuration-wait"),
            ]),
            sse(vec![
                ev_response_created("pending-configuration-ready"),
                ev_function_call(
                    LOGIN_CALL_ID,
                    "exec_command",
                    r#"{"cmd":"pwd","login":true}"#,
                ),
                ev_completed("pending-configuration-ready"),
            ]),
            sse(vec![
                ev_assistant_message("pending-configuration-message", "done"),
                ev_completed("pending-configuration-done"),
            ]),
            sse(vec![
                ev_function_call(EXEC_CALL_ID, "exec_command", r#"{"cmd":"pwd"}"#),
                ev_function_call(STDIN_CALL_ID, "write_stdin", r#"{"session_id":1}"#),
                ev_completed("failed-environment-exec"),
            ]),
            sse(vec![
                ev_assistant_message("failed-environment-message", "waiting for an environment"),
                ev_completed("failed-environment-done"),
            ]),
            sse(vec![
                ev_assistant_message("recovered-environment-message", "ready"),
                ev_completed("recovered-environment-done"),
            ]),
        ],
    )
    .await;
    empty
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "run a command without an attached environment".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&empty.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let empty_requests = response_mock.requests();
    let first_tool_names = tool_names(&empty_requests[0].body_json());
    for tool in ["exec_command", "write_stdin", "apply_patch", "view_image"] {
        assert!(
            first_tool_names.contains(&tool.to_string()),
            "missing tool: {tool}"
        );
    }
    for call_id in [EXEC_CALL_ID, STDIN_CALL_ID] {
        let (output, _) = empty_requests[1]
            .function_call_output_content_and_success(call_id)
            .context("tool output should be model visible without an environment")?;
        assert_eq!(output.as_deref(), Some(WAIT_MESSAGE));
    }
    waiting
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "wait for environment configuration".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 4).await;
    let pending_requests = response_mock.requests();
    assert_eq!(
        tool_names(&pending_requests[2].body_json()),
        first_tool_names
    );
    assert!(first_tool_names.contains(&"wait_for_environment".to_string()));
    for call_id in [EXEC_CALL_ID, STDIN_CALL_ID] {
        let (output, _) = pending_requests[3]
            .function_call_output_content_and_success(call_id)
            .context("premature tool output should be model visible")?;
        assert_eq!(output.as_deref(), Some(WAIT_MESSAGE));
    }

    independent
        .thread
        .environment_ready(
            &pending_selection,
            owner_config("independent-root", /*allow_login_shell*/ true),
        )
        .await?;
    failed
        .thread
        .environment_failed(&pending_selection, "configuration unavailable".to_string())
        .await?;
    for (thread, configured) in [
        (&waiting.thread, false),
        (&independent.thread, true),
        (&failed.thread, false),
    ] {
        let snapshot = thread.config_snapshot().await;
        assert_eq!(snapshot.is_primary_environment_configured(), configured);
    }
    assert_eq!(
        failed.thread.environment_selections().await,
        vec![TurnEnvironmentSelection {
            config: EnvironmentConfigState::Failed("configuration unavailable".to_string()),
            ..pending_selection.clone()
        }]
    );
    let downgraded_environments = TurnEnvironmentSelections::new(
        test.config.cwd.clone(),
        vec![TurnEnvironmentSelection {
            config: EnvironmentConfigState::FromThread,
            workspace_roots: Vec::new(),
            ..pending_selection.clone()
        }],
    );
    for thread in [&waiting.thread, &independent.thread, &failed.thread] {
        let error = thread
            .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                environments: Some(downgraded_environments.clone().into_requests()),
                ..Default::default()
            })
            .await
            .expect_err("owner-controlled environment must not become thread-owned");
        assert!(error.to_string().contains("owner-provided"));
    }
    let error = independent
        .thread
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "attempt to clear owner configuration".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(downgraded_environments.into_requests()),
                ..Default::default()
            }),
        )
        .await
        .expect_err("turn settings must not clear owner configuration");
    assert!(error.to_string().contains("owner-provided"));
    assert!(
        waiting
            .thread
            .inspect_selected_capability_roots()
            .ready_roots
            .is_empty()
    );

    let waiting_config = owner_config("waiting-root", /*allow_login_shell*/ false);
    let ready_selection = TurnEnvironmentSelection {
        config: EnvironmentConfigState::Ready(waiting_config.clone()),
        ..pending_selection.clone()
    };
    waiting
        .thread
        .environment_ready(&pending_selection, waiting_config)
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 6).await;
    wait_for_event(&waiting.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        waiting.thread.environment_selections().await,
        vec![ready_selection]
    );
    assert_eq!(
        waiting.thread.config_snapshot().await.workspace_roots,
        requested_workspace_roots
    );

    let ready_requests = response_mock.requests();
    let ready_request = &ready_requests[4];
    let (wait_output, wait_succeeded) = ready_request
        .function_call_output_content_and_success(WAIT_CALL_ID)
        .context("wait_for_environment output should be model visible")?;
    assert_ne!(wait_succeeded, Some(false));
    assert_eq!(
        serde_json::from_str::<Value>(&wait_output.context("wait output should contain JSON")?)?,
        json!({ "environment_id": selection.environment_id, "status": "ready" })
    );
    let body = ready_request.body_json();
    assert_eq!(tool_names(&body), first_tool_names);
    let (login_output, _) = ready_requests[5]
        .function_call_output_content_and_success(LOGIN_CALL_ID)
        .context("login policy error should be model visible")?;
    assert_eq!(
        login_output.as_deref(),
        Some("login shell is disabled by config; omit `login` or set it to false.")
    );
    assert_eq!(
        ready_request
            .message_input_texts("user")
            .into_iter()
            .rfind(|text| text.contains("<ready_capability_roots>")),
        Some("<ready_capability_roots>waiting-root</ready_capability_roots>".to_string())
    );
    assert!(
        ready_request
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains(&owner_workspace_root.inferred_native_path_string())),
        "waiting turn should observe owner-resolved workspace roots"
    );

    failed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "run a command after environment startup failed".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&failed.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let failed_requests = response_mock.requests();
    assert_eq!(
        tool_names(&failed_requests[6].body_json()),
        first_tool_names
    );
    for call_id in [EXEC_CALL_ID, STDIN_CALL_ID] {
        let (output, _) = failed_requests[7]
            .function_call_output_content_and_success(call_id)
            .context("tool output should be model visible after startup failed")?;
        assert_eq!(output.as_deref(), Some(WAIT_MESSAGE));
    }

    let recovered_selection = TurnEnvironmentSelection {
        config: EnvironmentConfigState::Ready(owner_config(
            "recovered-root",
            /*allow_login_shell*/ true,
        )),
        ..pending_selection
    };
    submit_thread_settings(
        &failed.thread,
        ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                test.config.cwd.clone(),
                vec![recovered_selection.clone().into_request()],
            )),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        failed.thread.environment_selections().await,
        vec![recovered_selection]
    );

    failed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "continue after environment recovery".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&failed.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let exec_command_parameters = response_mock
        .requests()
        .iter()
        .map(|request| {
            let body = request.body_json();
            let exec_command = body["tools"]
                .as_array()
                .context("tools should be an array")?
                .iter()
                .find(|tool| tool["name"] == "exec_command")
                .context("exec_command should stay available")?;
            let parameters = &exec_command["parameters"];
            parameters["properties"]
                .get("login")
                .context("login should be advertised regardless of environment state or policy")?;
            parameters["properties"].get("shell").context(
                "shell should be advertised regardless of environment state or shell mode",
            )?;
            Ok(parameters.clone())
        })
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        exec_command_parameters,
        vec![exec_command_parameters[0].clone(); exec_command_parameters.len()]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_pending_environment_can_finish_without_retargeting_the_active_turn() -> Result<()> {
    let server = start_mock_server().await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(WaitForEnvironmentTestExtension));
    extensions.prompt_contributor(Arc::new(ReadyCapabilityRootsTestExtension::default()));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::DeferredExecutor);
        });
    let test = builder.build_with_auto_env(&server).await?;
    let selection = test.codex.environment_selections().await.remove(0);
    let active = TurnEnvironmentSelection {
        config: EnvironmentConfigState::Pending,
        ..selection
    };
    let future_cwd = active.cwd.join("future-environment")?;
    test.fs()
        .create_directory(
            &future_cwd,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: true,
            },
        )
        .await?;
    let future = TurnEnvironmentSelection {
        cwd: future_cwd.clone(),
        workspace_roots: vec![future_cwd],
        ..active.clone()
    };
    let owner_config = |selection: &TurnEnvironmentSelection, id: &str| EnvironmentConfig {
        allow_login_shell: test.config.allow_login_shell,
        workspace_roots: selection.workspace_roots.clone(),
        shell_environment_policy: Default::default(),
        selected_capability_roots: vec![SelectedCapabilityRoot {
            id: id.to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: selection.environment_id.clone(),
                path: selection.cwd.clone(),
            },
        }],
    };
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![active.clone().into_request()]),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call(
                    "wait-for-active-environment",
                    "wait_for_environment",
                    &json!({ "environment_id": active.environment_id }).to_string(),
                ),
                ev_completed("waiting"),
            ]),
            sse(vec![ev_completed("active-done")]),
            sse(vec![ev_completed("future-done")]),
        ],
    )
    .await;
    let request = |text: &str| {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }])
    };
    thread.start_or_steer_turn(request("wait")).await?;
    // The HTTP request can arrive before its tool call is recorded. Steering
    // at that point can preempt the response and discard the wait call.
    wait_for_event(&thread, |event| {
        matches!(event, EventMsg::RawResponseItem(raw)
            if matches!(&raw.item, ResponseItem::FunctionCall { call_id, .. }
                if call_id == "wait-for-active-environment"))
    })
    .await;
    assert!(matches!(
        thread
            .start_or_steer_turn(
                request("use the other environment next turn").with_thread_settings(
                    ThreadSettingsOverrides {
                        environments: Some(
                            TurnEnvironmentSelections::new(
                                test.config.cwd.clone(),
                                vec![future.clone()],
                            )
                            .into_requests()
                        ),
                        ..Default::default()
                    },
                )
            )
            .await?,
        TurnInputSubmission::Steered { .. }
    ));
    thread
        .environment_ready(&future, owner_config(&future, "future-root"))
        .await?;
    assert!(matches!(
        thread.environment_selections().await[0].config,
        EnvironmentConfigState::Ready(_)
    ));
    thread
        .environment_ready(&active, owner_config(&active, "active-root"))
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    thread.start_or_steer_turn(request("next turn")).await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = response_mock.requests();
    let (output, _) = requests[1]
        .function_call_output_content_and_success("wait-for-active-environment")
        .context("active wait output should be model visible")?;
    assert_eq!(
        serde_json::from_str::<Value>(&output.context("active wait output should contain JSON")?)?,
        json!({ "environment_id": active.environment_id, "status": "ready" })
    );
    let ready_roots = requests[1..]
        .iter()
        .map(|request| {
            request
                .message_input_texts("user")
                .into_iter()
                .rfind(|text| text.contains("<ready_capability_roots>"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        ready_roots,
        ["active-root", "future-root"].map(|root| Some(format!(
            "<ready_capability_roots>{root}</ready_capability_roots>"
        )))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_environment_update_wakes_the_old_wait_with_the_new_selection() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex_with_wait_for_environment().with_config(|config| {
        config.features.enable(Feature::DeferredExecutor);
        config.features.disable(Feature::StepModelSwitching);
    });
    let test = builder.build_with_auto_env(&server).await?;
    let mut original = test.codex.environment_selections().await.remove(0);
    original.config = EnvironmentConfigState::Pending;
    let switched_cwd = original.cwd.join("active-environment")?;
    test.fs()
        .create_directory(
            &switched_cwd,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: true,
            },
        )
        .await?;
    let switched = TurnEnvironmentSelection {
        cwd: switched_cwd.clone(),
        workspace_roots: vec![switched_cwd],
        ..original.clone()
    };
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![original.clone().into_request()]),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    let wait = |id: &str| {
        sse(vec![
            ev_function_call(
                id,
                "wait_for_environment",
                &json!({ "environment_id": original.environment_id }).to_string(),
            ),
            ev_completed(id),
        ])
    };
    let responses = mount_sse_sequence(
        &server,
        vec![
            wait("wait-for-original"),
            wait("wait-for-switched"),
            sse(vec![ev_completed("active-done")]),
        ],
    )
    .await;
    let TurnInputSubmission::Started { turn_id, .. } = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "use the selected environment".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?
    else {
        anyhow::bail!("expected to start a turn");
    };
    wait_for_response_request_count(&responses, /*expected_count*/ 1).await;

    let (reply, outcome) = tokio::sync::oneshot::channel();
    thread
        .submit(Op::TurnSettings {
            turn_id,
            update: TurnSettingsUpdate {
                environments: Some(vec![switched.clone().into_request()]),
                ..Default::default()
            },
            reply,
        })
        .await?;
    assert_eq!(outcome.await?, TurnSettingsUpdateOutcome::Applied);
    wait_for_response_request_count(&responses, /*expected_count*/ 2).await;
    thread
        .environment_ready(
            &switched,
            environment_config_for_selection(&test.config, &switched),
        )
        .await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = responses.requests();
    let wait_output = |request: &ResponsesRequest, id| -> Result<String> {
        let (output, _) = request
            .function_call_output_content_and_success(id)
            .context("wait output should be model visible")?;
        output.context("wait output should contain text")
    };
    assert!(wait_output(&requests[1], "wait-for-original")?.contains("configuration was canceled"));
    assert_eq!(
        serde_json::from_str::<Value>(&wait_output(&requests[2], "wait-for-switched")?)?,
        json!({ "environment_id": original.environment_id, "status": "ready" })
    );
    for (request, selection, is_starting) in [
        (&requests[0], &original, true),
        (&requests[1], &switched, true),
        (&requests[2], &switched, false),
    ] {
        let context = request
            .message_input_texts("user")
            .into_iter()
            .rfind(|text| text.contains("<environment_context>"))
            .context("environment context should be model visible")?;
        assert!(context.contains(&format!(
            "<cwd>{}</cwd>",
            selection.cwd.inferred_native_path_string()
        )));
        assert_eq!(context.contains("<status>starting</status>"), is_starting);
    }
    Ok(())
}

#[test_case(true, false; "uses refreshed executor root")]
#[test_case(false, false; "preserves persisted root when executor reports none")]
#[test_case(true, true; "uses refreshed owner root")]
#[test_case(false, true; "preserves persisted root when owner reports none")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_before_selection_resolves_resumed_thread_capability_root_after_wait(
    reports_refreshed_root: bool,
    owner_provided: bool,
) -> Result<()> {
    const WAIT_CALL_ID: &str = "wait-ready-before-selection";

    let rendezvous = TcpListener::bind("127.0.0.1:0").await?;
    let rendezvous_url = format!("ws://{}", rendezvous.local_addr()?);
    let registry = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/cloud/environment/{REMOTE_ENVIRONMENT_ID}/register"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "environment_id": REMOTE_ENVIRONMENT_ID,
            "url": format!("{rendezvous_url}/relay?role=environment"),
            "security_profile": "noise_hybrid_ik_v1",
            "executor_registration_id": "ready-first-registration",
        })))
        .expect(1)
        .mount(&registry)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/cloud/environment/{REMOTE_ENVIRONMENT_ID}/validate"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "valid": true })))
        .expect(1)
        .mount(&registry)
        .await;

    let remote_config = RemoteEnvironmentConfig::new(
        registry.uri(),
        REMOTE_ENVIRONMENT_ID.to_string(),
        Arc::new(NoopRegistryAuthProvider),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )?;
    let remote_environment = tokio::spawn(codex_exec_server::run_remote_environment(remote_config));
    let (environment_socket, _) = timeout(Duration::from_secs(5), rendezvous.accept())
        .await
        .context("remote environment should reach rendezvous")??;
    let environment_websocket = timeout(Duration::from_secs(5), accept_async(environment_socket))
        .await
        .context("remote environment websocket handshake should complete")??;
    let executor_public_key = registry
        .received_requests()
        .await
        .context("wiremock should retain registration requests")?
        .iter()
        .find(|request| request.url.path().ends_with("/register"))
        .context("remote environment should register its public key")
        .and_then(|request| {
            serde_json::from_slice::<Value>(&request.body).context("registration request body")
        })
        .and_then(|body| {
            serde_json::from_value(body["executor_public_key"].clone())
                .context("registered executor public key")
        })?;

    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("ready-first-wait"),
                ev_function_call(
                    WAIT_CALL_ID,
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("ready-first-wait"),
            ]),
            sse(vec![
                ev_response_created("ready-first-done"),
                ev_assistant_message("ready-first-message", "done"),
                ev_completed("ready-first-done"),
            ]),
        ],
    )
    .await;
    let observed_roots = Arc::new(Mutex::new(Vec::new()));
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(WaitForEnvironmentTestExtension));
    extensions.prompt_contributor(Arc::new(ReadyCapabilityRootsTestExtension {
        observed_roots: Some(Arc::clone(&observed_roots)),
    }));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.project_doc_max_bytes = 0;
            config.features.enable(Feature::DeferredExecutor);
        });
    let test = builder.build(&server).await?;
    let refreshed_root = SelectedCapabilityRoot {
        id: "ready-first-root".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            path: PathUri::parse("file:///ready-first-root")?,
        },
    };
    let stale_root = SelectedCapabilityRoot {
        id: refreshed_root.id.clone(),
        location: CapabilityRootLocation::Environment {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            path: PathUri::parse("file:///stale-ready-first-root")?,
        },
    };
    let expected_root = if reports_refreshed_root {
        refreshed_root.clone()
    } else {
        stale_root.clone()
    };
    let mut executor_root = refreshed_root.clone();
    if owner_provided {
        // Owner configuration must not inherit the executor's shared root location.
        let CapabilityRootLocation::Environment { path, .. } = &mut executor_root.location;
        *path = PathUri::parse("file:///shared-root-not-selected-by-owner")?;
    }
    let provider = Arc::new(OfflineThenReadyNoiseConnectProvider {
        websocket_url: format!("{rendezvous_url}/relay?role=harness"),
        executor_public_key,
        calls: AtomicUsize::new(0),
    });
    let environment = test
        .thread_manager
        .environment_manager()
        .report_environment_provisioning_status(
            REMOTE_ENVIRONMENT_ID.to_string(),
            Ok(EnvironmentReadyInfo {
                selected_capability_roots: if reports_refreshed_root || owner_provided {
                    vec![executor_root]
                } else {
                    Vec::new()
                },
            }),
            provider.clone(),
        )?
        .context("Ready-first report should create the environment")?;

    assert!(!environment.startup_finished());
    let relay = tokio::spawn(async move {
        let (harness_socket, _) = timeout(Duration::from_secs(5), rendezvous.accept())
            .await
            .context("selecting the ready environment should start its Noise connection")??;
        let harness_websocket = timeout(Duration::from_secs(5), accept_async(harness_socket))
            .await
            .context("harness websocket handshake should complete")??;
        let mut environment_websocket = environment_websocket;
        let mut harness_websocket = harness_websocket;
        loop {
            tokio::select! {
                message = environment_websocket.next() => {
                    let Some(message) = message else {
                        break;
                    };
                    harness_websocket.send(message?).await?;
                }
                message = harness_websocket.next() => {
                    let Some(message) = message else {
                        break;
                    };
                    environment_websocket.send(message?).await?;
                }
            }
        }
        anyhow::Ok(())
    });

    let mut selection = TurnEnvironmentSelection::new(
        TurnEnvironmentRequest {
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&test.config.cwd),
            workspace_roots: vec![PathUri::from_abs_path(&test.config.cwd)],
            config: EnvironmentConfigState::FromThread,
        },
        std::slice::from_ref(&stale_root),
    );
    if owner_provided {
        let mut config = environment_config_for_selection(&test.config, &selection);
        if reports_refreshed_root {
            config.selected_capability_roots = vec![refreshed_root];
        }
        selection.config = EnvironmentConfigState::Ready(config);
    }
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(vec![stale_root]);
    let resumed = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![selection.clone().into_request()]),
            thread_extension_init,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    resumed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "use the ready environment".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_eq!(
        resumed.thread.environment_selections().await,
        vec![selection]
    );
    assert_eq!(
        resumed
            .thread
            .inspect_selected_capability_roots()
            .ready_roots,
        vec![expected_root.clone()]
    );

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
    // Provisioning was reported ready before selection, but selection materialization remains
    // nonblocking while the transport starts.
    // The first request may legally see either Starting or Ready; the wait makes step two ready.
    let first_tools = tool_names(&requests[0].body_json());
    assert!(first_tools.contains(&"wait_for_environment".to_string()));
    let first_user_context = requests[0].message_input_texts("user");
    let first_environment_context = first_user_context
        .iter()
        .rfind(|text| text.contains("<environment_context>"))
        .context("initial environment context should be model visible")?;
    let first_has_ready_root = first_user_context
        .iter()
        .any(|text| text.contains("<ready_capability_roots>ready-first-root"));
    let first_is_starting = first_environment_context.contains("<status>starting</status>");
    assert_eq!(
        first_tools.contains(&"exec_command".to_string()),
        !first_is_starting,
        "without stable_environment_tools, commands require a ready transport"
    );
    if first_is_starting {
        assert!(!first_has_ready_root);
    } else {
        assert!(first_environment_context.contains("<shell>"));
        assert!(first_has_ready_root);
    }

    let (_, wait_succeeded) = requests[1]
        .function_call_output_content_and_success(WAIT_CALL_ID)
        .context("wait_for_environment output should be model visible")?;
    assert_ne!(wait_succeeded, Some(false));
    assert!(tool_names(&requests[1].body_json()).contains(&"exec_command".to_string()));
    let user_context = requests[1].message_input_texts("user");
    let environment_context = user_context
        .iter()
        .rfind(|text| text.contains("<environment_context>"))
        .context("ready environment context should be model visible")?;
    assert!(!environment_context.contains("status=\"unavailable\""));
    assert!(!environment_context.contains("<status>starting</status>"));
    assert!(environment_context.contains("<shell>"));
    assert!(
        user_context
            .iter()
            .any(|text| text.contains("<ready_capability_roots>ready-first-root"))
    );
    let observed_selected_roots = observed_roots
        .lock()
        .expect("observed capability roots should not be poisoned")
        .iter()
        .rfind(|roots| roots.iter().any(|root| root.id == expected_root.id))
        .cloned()
        .context("selected capability root should reach world state")?;
    assert_eq!(observed_selected_roots, vec![expected_root]);

    relay.abort();
    remote_environment.abort();
    let _ = relay.await;
    let _ = remote_environment.await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_stays_pending_after_materialization() -> Result<()> {
    let server = start_mock_server().await;
    let wait_call_id = "wait-for-startup";
    let response_mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(
                wait_call_id,
                "wait_for_environment",
                &json!({
                    "environment_id": REMOTE_ENVIRONMENT_ID,
                })
                .to_string(),
            ),
            ev_completed("resp-1"),
        ])],
    )
    .await;
    let mut builder = test_codex_with_wait_for_environment().with_config(|config| {
        config.features.enable(Feature::DeferredExecutor);
    });
    let test = expect_startup(builder.build(&server)).await;
    let environment_manager = test.thread_manager.environment_manager();
    let provider = Arc::new(FailingNoiseConnectProvider::default());
    environment_manager.materialize_pending_noise_environment(
        REMOTE_ENVIRONMENT_ID.to_string(),
        provider.clone(),
    )?;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "wait for the environment".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![TurnEnvironmentRequest {
                        environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                        cwd: PathUri::from_abs_path(&test.config.cwd),
                        workspace_roots: vec![PathUri::from_abs_path(&test.config.cwd)],
                        config: EnvironmentConfigState::FromThread,
                    }],
                )),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    assert_eq!(provider.calls.load(Ordering::Relaxed), 0);

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 1);
    let starting_request_body = requests[0].body_json();
    let starting_tools = tool_names(&starting_request_body);
    assert!(starting_tools.contains(&"wait_for_environment".to_string()));
    let wait_tool = starting_request_body["tools"]
        .as_array()
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool["name"] == "wait_for_environment")
        })
        .context("wait_for_environment tool schema should be present")?;
    assert_eq!(
        wait_tool["description"].as_str(),
        Some(WAIT_FOR_ENVIRONMENT_TEST_TOOL_DESCRIPTION)
    );
    assert_eq!(
        wait_tool["parameters"]["properties"]["environment_id"]["description"].as_str(),
        Some(WAIT_FOR_ENVIRONMENT_TEST_ENVIRONMENT_ID_DESCRIPTION)
    );

    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;

    Ok(())
}

#[test_case(false, "multi_agent_v1"; "v1")]
#[test_case(true, "collaboration"; "v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_spawn_agent_inherits_ready_step_environments(
    multi_agent_v2: bool,
    namespace: &str,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let wait_call_id = "wait-for-spawn-environment";
    let spawn_call_id = "spawn-in-ready-environment";
    let message = "inspect the ready step environment";
    let spawn_arguments = if multi_agent_v2 {
        json!({ "message": message, "task_name": "worker" })
    } else {
        json!({ "message": message })
    }
    .to_string();
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-wait"),
                ev_function_call(
                    wait_call_id,
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-wait"),
            ]),
            sse(vec![
                ev_response_created("resp-spawn"),
                ev_function_call_with_namespace(
                    spawn_call_id,
                    namespace,
                    "spawn_agent",
                    &spawn_arguments,
                ),
                ev_completed("resp-spawn"),
            ]),
            sse(vec![
                ev_response_created("resp-done-1"),
                ev_assistant_message("msg-done-1", "done"),
                ev_completed("resp-done-1"),
            ]),
            sse(vec![
                ev_response_created("resp-done-2"),
                ev_assistant_message("msg-done-2", "done"),
                ev_completed("resp-done-2"),
            ]),
        ],
    )
    .await;
    // If the child finishes after the parent's last request, its completion
    // notification triggers one more parent request. Allow either ordering.
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-child-completion"),
            ev_assistant_message("msg-child-completion", "done"),
            ev_completed("resp-child-completion"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(move |config| {
            config.project_doc_max_bytes = 0;
            config.features.enable(Feature::DeferredExecutor);
            config.features.enable(Feature::Collab);
            if multi_agent_v2 {
                config.features.enable(Feature::MultiAgentV2);
            } else {
                config.features.disable(Feature::MultiAgentV2);
            }
        });
    let (attach_tx, attach_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let exec_server = tokio::spawn(serve_environment_with_agents_md(
        listener,
        "",
        attach_rx,
        shutdown_rx,
    ));
    let test = expect_startup(builder.build_with_remote_and_local_env(&server)).await;
    let remote_selection = TurnEnvironmentSelection {
        selected_capability_roots: Default::default(),
        environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(&test.config.cwd),
        workspace_roots: vec![PathUri::from_abs_path(&test.config.cwd)],
        config: EnvironmentConfigState::Ready(EnvironmentConfig {
            allow_login_shell: test.config.allow_login_shell,
            workspace_roots: vec![PathUri::from_abs_path(&test.config.cwd)],
            shell_environment_policy: Default::default(),
            selected_capability_roots: Vec::new(),
        }),
    };
    let expected_environments = vec![remote_selection, local(test.config.cwd.clone())];
    let mut created_threads = test.thread_manager.subscribe_thread_created();

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "spawn after the environment becomes ready".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    expected_environments
                        .clone()
                        .into_iter()
                        .map(TurnEnvironmentSelection::into_request)
                        .collect(),
                )),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    attach_tx.send(()).expect("attach remote environment");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 4).await;

    let child_thread_id = timeout(Duration::from_secs(5), created_threads.recv())
        .await
        .context("timed out waiting for the subagent thread")??;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    assert_eq!(
        child_thread.environment_selections().await,
        expected_environments
    );
    assert!(
        response_mock.requests()[1]
            .function_call_output_content_and_success(wait_call_id)
            .is_some(),
        "the spawn request should follow the ready-environment step"
    );

    shutdown_tx
        .send(())
        .expect("stop remote environment server");
    exec_server.await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_loads_agents_md_when_environment_becomes_ready() -> Result<()> {
    const AGENTS_CONTENT: &str = "REMOTE_AGENTS_INSTRUCTIONS";

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "wait-1",
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(
                    "wait-2",
                    "wait_for_environment",
                    &json!({ "environment_id": REMOTE_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex_with_wait_for_environment()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            config.features.enable(Feature::DeferredExecutor);
        });
    let (attach_tx, attach_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let exec_server = tokio::spawn(serve_environment_with_agents_md(
        listener,
        AGENTS_CONTENT,
        attach_rx,
        shutdown_rx,
    ));
    let test = expect_startup(builder.build(&server)).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "load the environment instructions".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_response_request_count(&response_mock, /*expected_count*/ 1).await;
    let agents_path = PathUri::from_abs_path(&test.config.cwd).join("AGENTS.md")?;
    attach_tx.send(()).expect("attach environment");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    shutdown_tx.send(()).expect("stop exec server");
    let agents_md_reads = exec_server.await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(agents_md_reads, 1);
    assert_eq!(agents_md_occurrences(&requests[0], AGENTS_CONTENT), 0);
    assert_eq!(agents_md_occurrences(&requests[1], AGENTS_CONTENT), 1);
    assert_eq!(agents_md_occurrences(&requests[2], AGENTS_CONTENT), 1);
    assert_eq!(environment_instructions_occurrences(&requests[0]), 1);
    assert_eq!(environment_instructions_occurrences(&requests[1]), 1);
    assert_eq!(environment_instructions_occurrences(&requests[2]), 1);
    assert_eq!(test.codex.instruction_sources().await, vec![agents_path]);

    Ok(())
}

fn agents_md_occurrences(request: &ResponsesRequest, contents: &str) -> usize {
    request
        .message_input_texts("user")
        .iter()
        .filter(|text| text.contains(contents))
        .count()
}

fn environment_instructions_occurrences(request: &ResponsesRequest) -> usize {
    request
        .message_input_texts("developer")
        .iter()
        .filter(|text| text.contains(ENVIRONMENTS_INSTRUCTIONS_OPEN_TAG))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_executor_compaction_preserves_then_updates_environment_once() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "wait-for-startup",
                    "request_user_input",
                    &json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue after startup?",
                            "options": [{
                                "label": "Yes (Recommended)",
                                "description": "Continue the test."
                            }, {
                                "label": "No",
                                "description": "Stop the test."
                            }]
                        }]
                    })
                    .to_string(),
                ),
                ev_completed_with_tokens("resp-1", /*total_tokens*/ 96),
            ]),
            sse(vec![
                ev_assistant_message("msg-compact", "AUTO_COMPACT_SUMMARY"),
                ev_completed_with_tokens("resp-compact", /*total_tokens*/ 10),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex_with_wait_for_environment()
        .with_exec_server_url(format!("ws://{}", listener.local_addr()?))
        .with_config(|config| {
            config.project_doc_max_bytes = 0;
            config.features.enable(Feature::DeferredExecutor);
            config.features.enable(Feature::DefaultModeRequestUserInput);
            config.model_provider.name = "OpenAI (test)".to_string();
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_context_window = Some(100);
            config.model_auto_compact_token_limit = Some(90);
        });
    let test = expect_startup(builder.build(&server)).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "wait for the environment".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;

    serve_environment_info(listener).await;
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "continue".to_string(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes (Recommended)".to_string()],
                    },
                )]),
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let initial_context = requests[0].message_input_texts("user");
    assert!(
        initial_context
            .iter()
            .any(|text| text.contains("<status>starting</status>"))
    );

    let post_compaction_context = requests[2].message_input_texts("user");
    assert_eq!(
        post_compaction_context
            .iter()
            .filter(|text| text.contains("<status>starting</status>"))
            .count(),
        1
    );
    assert_eq!(
        post_compaction_context
            .iter()
            .filter(|text| text.contains("<shell>zsh</shell>"))
            .count(),
        1
    );
    let starting_index = post_compaction_context
        .iter()
        .position(|text| text.contains("<status>starting</status>"))
        .expect("compaction should preserve the prior environment state");
    let ready_index = post_compaction_context
        .iter()
        .position(|text| text.contains("<shell>zsh</shell>"))
        .expect("the next sampling step should report that the environment is ready");
    assert!(starting_index < ready_index);

    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let rollout_path = test.codex.rollout_path().context("rollout path")?;
    let rollout = fs::read_to_string(rollout_path)?;
    let world_state_items = rollout
        .lines()
        .map(codex_rollout::parse_rollout_line)
        .collect::<serde_json::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|line| match line.item {
            RolloutItem::WorldState(item) => Some(item),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        world_state_items
            .iter()
            .map(|item| item.full)
            .collect::<Vec<_>>(),
        vec![true, true, false]
    );
    assert_eq!(
        world_state_items[0].state["environments"].pointer("/environments/remote/status"),
        Some(&json!("starting"))
    );
    assert_eq!(
        world_state_items[2].state["environments"].pointer("/environments/remote/status"),
        Some(&json!("available"))
    );
    assert_eq!(
        world_state_items[2].state["environments"].pointer("/environments/remote/shell"),
        Some(&json!("zsh"))
    );

    Ok(())
}

fn absolute_path(path: PathBuf) -> AbsolutePathBuf {
    AbsolutePathBuf::try_from(path).expect("path should be absolute")
}

fn remote_exec(script: &str) -> Result<()> {
    let container_name = test_docker_container_name()
        .context("test requires direct access to the Docker container")?;
    let output = Command::new("docker")
        .args(["exec", container_name.as_str(), "sh", "-lc", script])
        .output()?;
    assert!(
        output.status.success(),
        "remote exec failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    );
    Ok(())
}

async fn exec_command_routing_output(
    test: &TestCodex,
    server: &wiremock::MockServer,
    call_id: &str,
    arguments: Value,
    environments: Option<Vec<TurnEnvironmentSelection>>,
) -> Result<String> {
    let response_mock = mount_sse_sequence(
        server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "exec_command", &serde_json::to_string(&arguments)?),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_environments("route exec command", environments)
        .await?;

    let output = response_mock
        .function_call_output_text(call_id)
        .with_context(|| format!("missing function_call_output for {call_id}"))?;
    let request = response_mock
        .requests()
        .into_iter()
        .next()
        .context("initial model request should be recorded")?;
    let tools = tool_names(&request.body_json());
    assert!(tools.contains(&"exec_command".to_string()));
    assert!(tools.contains(&"write_stdin".to_string()));

    Ok(output)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_routes_to_selected_remote_environment() -> Result<()> {
    skip_if_no_network!(Ok(()));
    // TODO(anp): Remove after remote path fixtures use target-native paths.
    skip_if_target_windows!(Ok(()), "requires the Docker-backed POSIX executor");
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let test = unified_exec_test(&server).await?;
    let local_cwd = TempDir::new()?;
    fs::write(local_cwd.path().join("marker.txt"), "local-routing")?;
    let local_selection = local(local_cwd.path().abs());
    let remote_cwd = PathBuf::from(format!(
        "/tmp/codex-remote-routing-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ))
    .abs();
    let remote_marker_name = "marker.txt";
    let remote_cwd_uri = PathUri::from_host_native_path(&remote_cwd)?;
    let remote_marker_uri = PathUri::from_host_native_path(remote_cwd.join(remote_marker_name))?;
    test.fs()
        .create_directory(
            &remote_cwd_uri,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
        )
        .await?;
    test.fs()
        .write_file(
            &remote_marker_uri,
            b"remote-routing".to_vec(),
            Default::default(),
        )
        .await?;
    let remote_selection = TurnEnvironmentSelection {
        selected_capability_roots: Default::default(),
        environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
        cwd: PathUri::from_abs_path(&remote_cwd),
        workspace_roots: vec![PathUri::from_abs_path(&remote_cwd)],
        config: EnvironmentConfigState::FromThread,
    };
    let multi_env_output = exec_command_routing_output(
        &test,
        &server,
        "call-multi-env",
        json!({
            "shell": "/bin/sh",
            "cmd": format!("cat {remote_marker_name}"),
            "login": false,
            "yield_time_ms": 1_000,
            "environment_id": REMOTE_ENVIRONMENT_ID,
        }),
        Some(vec![local_selection, remote_selection]),
    )
    .await?;
    assert!(
        multi_env_output.contains("remote-routing"),
        "unexpected multi-env output: {multi_env_output}",
    );
    assert!(
        !multi_env_output.contains("local-routing"),
        "multi-env command should not route to local: {multi_env_output}",
    );

    test.fs()
        .remove(
            &remote_cwd_uri,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_freeform_routes_to_selected_remote_environment() -> Result<()> {
    skip_if_no_network!(Ok(()));
    // TODO(anp): Remove after remote path fixtures use target-native paths.
    skip_if_target_windows!(Ok(()), "requires the Docker-backed POSIX executor");
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let local_cwd = TempDir::new()?;
    let file_name = "apply_patch_remote_freeform.txt";
    let remote_cwd = PathBuf::from(format!(
        "/tmp/codex-remote-apply-patch-freeform-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ))
    .abs();
    let remote_cwd_uri = PathUri::from_host_native_path(&remote_cwd)?;
    test.fs()
        .create_directory(
            &remote_cwd_uri,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
        )
        .await?;

    let patch = format!(
        "*** Begin Patch\n*** Environment ID: {REMOTE_ENVIRONMENT_ID}\n*** Add File: {file_name}\n+patched remote freeform\n*** End Patch"
    );
    let call_id = "apply-patch-remote-freeform";
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_apply_patch_custom_tool_call(call_id, &patch),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_environments(
        "apply patch to remote environment",
        Some(vec![
            local(local_cwd.path().abs()),
            TurnEnvironmentSelection {
                selected_capability_roots: Default::default(),
                environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                cwd: PathUri::from_abs_path(&remote_cwd),
                workspace_roots: vec![PathUri::from_abs_path(&remote_cwd)],
                config: EnvironmentConfigState::FromThread,
            },
        ]),
    )
    .await?;

    let remote_contents = test
        .fs()
        .read_file_text(
            &PathUri::from_host_native_path(remote_cwd.join(file_name))?,
            Default::default(),
        )
        .await?;
    assert_eq!(remote_contents, "patched remote freeform\n");
    assert!(
        !local_cwd.path().join(file_name).exists(),
        "freeform apply_patch should not create the file in the local environment"
    );

    test.fs()
        .remove(
            &remote_cwd_uri,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_intercepted_exec_command_routes_to_selected_remote_environment() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    // TODO(anp): Remove after remote path fixtures use target-native paths.
    skip_if_target_windows!(Ok(()), "requires the Docker-backed POSIX executor");
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let test = unified_exec_test(&server).await?;
    let local_cwd = TempDir::new()?;
    let file_name = "apply_patch_remote_exec.txt";
    let remote_cwd = PathBuf::from(format!(
        "/tmp/codex-remote-apply-patch-exec-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ))
    .abs();
    let remote_cwd_uri = PathUri::from_host_native_path(&remote_cwd)?;
    test.fs()
        .create_directory(
            &remote_cwd_uri,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
        )
        .await?;

    let patch =
        format!("*** Begin Patch\n*** Add File: {file_name}\n+patched remote exec\n*** End Patch");
    let command = format!("apply_patch <<'EOF'\n{patch}\nEOF\n");
    let call_id = "apply-patch-remote-exec";
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    call_id,
                    "exec_command",
                    &serde_json::to_string(&json!({
                        "shell": "/bin/sh",
                        "cmd": command,
                        "login": false,
                        "yield_time_ms": 5_000,
                        "environment_id": REMOTE_ENVIRONMENT_ID,
                    }))?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    test.submit_turn_with_environments(
        "apply patch through exec command to remote environment",
        Some(vec![
            local(local_cwd.path().abs()),
            TurnEnvironmentSelection {
                selected_capability_roots: Default::default(),
                environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                cwd: PathUri::from_abs_path(&remote_cwd),
                workspace_roots: vec![PathUri::from_abs_path(&remote_cwd)],
                config: EnvironmentConfigState::FromThread,
            },
        ]),
    )
    .await?;

    let remote_contents = test
        .fs()
        .read_file_text(
            &PathUri::from_host_native_path(remote_cwd.join(file_name))?,
            Default::default(),
        )
        .await?;
    assert_eq!(remote_contents, "patched remote exec\n");
    assert!(
        !local_cwd.path().join(file_name).exists(),
        "intercepted apply_patch should not create the file in the local environment"
    );

    test.fs()
        .remove(
            &remote_cwd_uri,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_remove_removes_symlink_not_target() -> Result<()> {
    skip_if_target_windows!(Ok(()), "tests POSIX symlink removal semantics");
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let test_env = test_env().await?;
    let file_system = test_env.environment().get_filesystem();

    let root = PathBuf::from(format!(
        "/tmp/codex-remote-remove-link-{}",
        std::process::id()
    ));
    let allowed_dir = root.join("allowed");
    let outside_file = root.join("outside").join("keep.txt");
    let symlink_path = allowed_dir.join("link");
    remote_exec(&format!(
        "rm -rf {root}; mkdir -p {allowed} {outside_parent}; printf outside > {outside}; ln -s {outside} {symlink}",
        root = root.display(),
        allowed = allowed_dir.display(),
        outside_parent = absolute_path(
            outside_file
                .parent()
                .context("outside parent should exist")?
                .to_path_buf(),
        )
        .display(),
        outside = outside_file.display(),
        symlink = symlink_path.display(),
    ))?;

    file_system
        .remove(
            &PathUri::from_host_native_path(&symlink_path)?,
            RemoveOptions {
                recursive: false,
                force: false,
                follow_symlinks: true,
            },
        )
        .await?;

    let symlink_exists = file_system
        .get_metadata(
            &PathUri::from_abs_path(&absolute_path(symlink_path)),
            Default::default(),
        )
        .await
        .is_ok();
    assert!(!symlink_exists);
    let outside = file_system
        .read_file_text(
            &PathUri::from_host_native_path(&outside_file)?,
            Default::default(),
        )
        .await?;
    assert_eq!(outside, "outside");

    file_system
        .remove(
            &PathUri::from_host_native_path(&root)?,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_test_env_copy_preserves_symlink_source() -> Result<()> {
    skip_if_target_windows!(Ok(()), "tests POSIX symlink copy semantics");
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let test_env = test_env().await?;
    let file_system = test_env.environment().get_filesystem();

    let root = PathBuf::from(format!(
        "/tmp/codex-remote-copy-link-{}",
        std::process::id()
    ));
    let allowed_dir = root.join("allowed");
    let outside_file = root.join("outside").join("outside.txt");
    let source_symlink = allowed_dir.join("link");
    let copied_symlink = allowed_dir.join("copied-link");
    remote_exec(&format!(
        "rm -rf {root}; mkdir -p {allowed} {outside_parent}; printf outside > {outside}; ln -s {outside} {source}",
        root = root.display(),
        allowed = allowed_dir.display(),
        outside_parent = outside_file.parent().expect("outside parent").display(),
        outside = outside_file.display(),
        source = source_symlink.display(),
    ))?;

    file_system
        .copy(
            &PathUri::from_host_native_path(&source_symlink)?,
            &PathUri::from_host_native_path(&copied_symlink)?,
            CopyOptions { recursive: false },
        )
        .await?;

    let container_name = test_docker_container_name()
        .context("test requires direct access to the Docker container")?;
    let link_target = Command::new("docker")
        .args([
            "exec",
            container_name.as_str(),
            "readlink",
            copied_symlink
                .to_str()
                .context("copied symlink path should be utf-8")?,
        ])
        .output()?;
    assert!(
        link_target.status.success(),
        "readlink failed: stdout={} stderr={}",
        String::from_utf8_lossy(&link_target.stdout).trim(),
        String::from_utf8_lossy(&link_target.stderr).trim(),
    );
    assert_eq!(
        String::from_utf8_lossy(&link_target.stdout).trim(),
        outside_file.to_string_lossy()
    );

    file_system
        .remove(
            &PathUri::from_host_native_path(&root)?,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;
    Ok(())
}

#[path = "remote_env_failure_tests.rs"]
mod failure_tests;

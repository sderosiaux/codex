use codex_exec_server::CreateDirectoryOptions;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_reasoning_item_added;
use core_test_support::responses::ev_reasoning_summary_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_delegate_ignores_legacy_deltas() {
    skip_if_no_network!();

    // Single response with reasoning summary deltas.
    let sse_stream = sse(vec![
        ev_response_created("resp-1"),
        ev_reasoning_item_added("reason-1", &["initial"]),
        ev_reasoning_summary_text_delta("think-1"),
        ev_completed("resp-1"),
    ]);

    let server = start_mock_server().await;
    mount_sse_sequence(&server, vec![sse_stream]).await;

    let mut builder = test_codex();
    let test = builder.build(&server).await.expect("build test codex");

    // Kick off review (delegated).
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Please review".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .expect("submit review");

    let mut reasoning_delta_count = 0;

    loop {
        let ev = wait_for_event(&test.codex, |_| true).await;
        match ev {
            EventMsg::ReasoningContentDelta(_) => reasoning_delta_count += 1,
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(reasoning_delta_count, 1, "expected one new reasoning delta");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_delegate_handles_skill_mcp_dependency_without_prompting() {
    skip_if_wine_exec!("skill paths require matching host and executor path conventions");
    skip_if_no_network!();

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::SkillMcpDependencyInstall);
        })
        .with_workspace_setup(|cwd, fs| async move {
            let skill_dir = cwd.join(".agents/skills/dependency-skill");
            let agents_dir = skill_dir.join("agents");
            fs.create_directory(
                &PathUri::from_host_native_path(&agents_dir)?,
                CreateDirectoryOptions { recursive: true, follow_symlinks: true }
            )
            .await?;
            fs.write_file(
                &PathUri::from_host_native_path(skill_dir.join("SKILL.md"))?,
                b"---\nname: dependency-skill\ndescription: Requires an MCP server.\n---\n\nReview dependency instructions.\n"
                    .to_vec(),
                Default::default()
            )
            .await?;
            fs.write_file(
                &PathUri::from_host_native_path(agents_dir.join("openai.yaml"))?,
                b"dependencies:\n  tools:\n    - type: mcp\n      value: missing-review-server\n      transport: streamable_http\n      url: http://127.0.0.1:1/mcp\n"
                    .to_vec(),
                Default::default()
            )
            .await?;
            Ok(())
        })
        .build_with_auto_env(&server)
        .await
        .expect("build review delegate with a missing skill MCP dependency");

    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "Review with $dependency-skill".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await
        .expect("submit review");

    let event = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::RequestUserInput(_) | EventMsg::TurnComplete(_)
        )
    })
    .await;
    assert!(
        matches!(event, EventMsg::TurnComplete(_)),
        "review delegate should handle skill MCP dependencies without prompting: {event:?}"
    );

    let request = response_mock.single_request();
    let user_texts = request.message_input_texts("user");
    assert!(
        user_texts.iter().any(|text| {
            text.contains("<skill>\n<name>dependency-skill</name>")
                && text.contains("Review dependency instructions.")
        }),
        "review should continue with the selected skill despite its unreachable MCP dependency: {user_texts:?}"
    );
}

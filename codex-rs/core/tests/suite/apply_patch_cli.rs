use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::TurnEnvironmentRequests;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_apply_patch_exec_command_call_via_heredoc;
use core_test_support::responses::ev_exec_command_call;
use core_test_support::test_codex::ApplyPatchModelOutput;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_exec_server::CreateDirectoryOptions;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_exec_server::RemoveOptions;
use codex_features::Feature;
#[cfg(unix)]
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::EventMsg;
#[cfg(unix)]
#[cfg(unix)]
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::user_input::UserInput;
#[cfg(target_os = "linux")]
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::PathBufExt;
use core_test_support::TestTargetOs;
use core_test_support::assert_regex_match;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_exec_command_call_with_args;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_remote;
use core_test_support::skip_if_target_windows;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::executor_path_uri;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use serde_json::json;
use test_case::test_case;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

pub async fn apply_patch_harness() -> Result<TestCodexHarness> {
    apply_patch_harness_with(|builder| builder).await
}

async fn apply_patch_harness_with(
    configure: impl FnOnce(TestCodexBuilder) -> TestCodexBuilder,
) -> Result<TestCodexHarness> {
    let builder = configure(test_codex());
    // Box harness construction so apply_patch_cli tests do not inline the
    // full test-thread startup path into each test future.
    Box::pin(TestCodexHarness::with_auto_env_builder(builder)).await
}

async fn submit_without_wait(harness: &TestCodexHarness, prompt: &str) -> Result<()> {
    let test = harness.test();
    let session_model = test.session_configured.model.clone();
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: session_model,
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

pub async fn mount_apply_patch(
    harness: &TestCodexHarness,
    call_id: &str,
    patch: &str,
    assistant_msg: &str,
) {
    mount_sse_sequence(
        harness.server(),
        apply_patch_responses(
            call_id,
            patch,
            assistant_msg,
            ev_apply_patch_custom_tool_call,
        ),
    )
    .await;
}

async fn mount_apply_patch_model_output(
    harness: &TestCodexHarness,
    call_id: &str,
    patch: &str,
    assistant_msg: &str,
    model_output: ApplyPatchModelOutput,
) {
    let apply_patch_call = match model_output {
        ApplyPatchModelOutput::ExecCommandViaHeredoc => {
            ev_apply_patch_exec_command_call_via_heredoc
        }
    };

    mount_sse_sequence(
        harness.server(),
        apply_patch_responses(call_id, patch, assistant_msg, apply_patch_call),
    )
    .await;
}

fn apply_patch_responses(
    call_id: &str,
    patch: &str,
    assistant_msg: &str,
    apply_patch_call: fn(&str, &str) -> serde_json::Value,
) -> Vec<String> {
    vec![
        sse(vec![
            ev_response_created("resp-1"),
            apply_patch_call(call_id, patch),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", assistant_msg),
            ev_completed("resp-2"),
        ]),
    ]
}

async fn assert_apply_patch_crlf_update(model_output: CrLfApplyPatchModelOutput) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    let call_id = "apply-patch-crlf";
    let file_name = "crlf.txt";
    harness.write_file(file_name, "before\r\n").await?;
    let patch = format!(
        "*** Begin Patch\n*** Update File: {file_name}\n@@\n-before\n+after\n*** End Patch\n"
    );
    match model_output {
        CrLfApplyPatchModelOutput::CustomTool => {
            mount_apply_patch(&harness, call_id, &patch, "apply_patch done").await;
        }
        CrLfApplyPatchModelOutput::ExecCommandViaHeredoc => {
            mount_apply_patch_model_output(
                &harness,
                call_id,
                &patch,
                "apply_patch done",
                ApplyPatchModelOutput::ExecCommandViaHeredoc,
            )
            .await;
        }
    }

    harness
        .test()
        .submit_turn("update the CRLF file with apply_patch")
        .await?;

    assert_eq!(harness.read_file_text(file_name).await?, "after\r\n");
    Ok(())
}

#[derive(Clone, Copy)]
enum CrLfApplyPatchModelOutput {
    CustomTool,
    ExecCommandViaHeredoc,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_preserves_crlf_by_default() -> Result<()> {
    assert_apply_patch_crlf_update(CrLfApplyPatchModelOutput::CustomTool).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_shell_heredoc_preserves_crlf_by_default() -> Result<()> {
    skip_if_wine_exec!(Ok(()), "uses a POSIX shell heredoc");
    assert_apply_patch_crlf_update(CrLfApplyPatchModelOutput::ExecCommandViaHeredoc).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_multiple_operations_integration() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| builder.with_model("gpt-5.5")).await?;

    // Seed workspace state
    harness.write_file("modify.txt", "line1\nline2\n").await?;
    harness.write_file("delete.txt", "obsolete\n").await?;

    let patch = "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Delete File: delete.txt\n*** Update File: modify.txt\n@@\n-line2\n+changed\n*** End Patch";

    let call_id = "apply-multi-ops";
    mount_apply_patch(&harness, call_id, patch, "done").await;

    harness.submit("please apply multi-ops patch").await?;

    let out = harness.apply_patch_output(call_id).await;

    let expected = r"(?s)^Exit code: 0
Wall time: [0-9]+(?:\.[0-9]+)? seconds
Output:
Success. Updated the following files:
A nested/new.txt
M modify.txt
D delete.txt
?$";
    assert_regex_match(expected, &out);

    assert_eq!(harness.read_file_text("nested/new.txt").await?, "created\n");
    assert_eq!(
        harness.read_file_text("modify.txt").await?,
        "line1\nchanged\n"
    );
    assert!(!harness.path_exists("delete.txt").await?);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_preserves_distinct_updated_paths() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    harness.write_file("first.txt", "first before\n").await?;
    harness.write_file("second.txt", "second before\n").await?;

    let patch = "*** Begin Patch\n*** Update File: first.txt\n@@\n-first before\n+first after\n*** Update File: second.txt\n@@\n-second before\n+second after\n*** End Patch";
    let call_id = "apply-distinct-updates";
    mount_apply_patch(&harness, call_id, patch, "done").await;

    harness.submit("please update both files").await?;

    assert_regex_match(
        r"(?s)^Exit code: 0.*Success\. Updated the following files:\nM first\.txt\nM second\.txt\n?$",
        &harness.apply_patch_output(call_id).await,
    );
    assert_eq!(harness.read_file_text("first.txt").await?, "first after\n");
    assert_eq!(
        harness.read_file_text("second.txt").await?,
        "second after\n"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_rejects_duplicate_resolved_paths() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    harness.write_file("duplicate.txt", "before\n").await?;

    let patch = "*** Begin Patch\n*** Update File: duplicate.txt\n@@\n-before\n+first after\n*** Update File: ./duplicate.txt\n@@\n-before\n+second after\n*** End Patch";
    let call_id = "apply-duplicate-resolved-path";
    mount_apply_patch(&harness, call_id, patch, "done").await;

    harness.submit("please apply both updates").await?;

    let out = harness.apply_patch_output(call_id).await;
    assert!(
        out.contains("apply_patch verification failed"),
        "expected verification failure: {out}"
    );
    assert!(
        out.contains("multiple operations target"),
        "expected duplicate-path diagnostics: {out}"
    );
    assert_eq!(harness.read_file_text("duplicate.txt").await?, "before\n");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_multiple_chunks() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness
        .write_file("multi.txt", "line1\nline2\nline3\nline4\n")
        .await?;

    let patch = "*** Begin Patch\n*** Update File: multi.txt\n@@\n-line2\n+changed2\n@@\n-line4\n+changed4\n*** End Patch";
    let call_id = "apply-multi-chunks";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply multi-chunk patch").await?;

    assert_eq!(
        harness.read_file_text("multi.txt").await?,
        "line1\nchanged2\nline3\nchanged4\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_moves_file_to_new_directory() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("old/name.txt", "old content\n").await?;

    let patch = "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-old content\n+new content\n*** End Patch";
    let call_id = "apply-move";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply move patch").await?;

    assert!(!harness.path_exists("old/name.txt").await?);
    assert_eq!(
        harness.read_file_text("renamed/dir/name.txt").await?,
        "new content\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_updates_file_appends_trailing_newline() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness
        .write_file("no_newline.txt", "no newline at end")
        .await?;

    let patch = "*** Begin Patch\n*** Update File: no_newline.txt\n@@\n-no newline at end\n+first line\n+second line\n*** End Patch";
    let call_id = "apply-append-nl";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply newline patch").await?;

    let contents = harness.read_file_text("no_newline.txt").await?;
    assert!(contents.ends_with('\n'));
    assert_eq!(contents, "first line\nsecond line\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_insert_only_hunk_modifies_file() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness
        .write_file("insert_only.txt", "alpha\nomega\n")
        .await?;

    let patch = "*** Begin Patch\n*** Update File: insert_only.txt\n@@\n alpha\n+beta\n omega\n*** End Patch";
    let call_id = "apply-insert-only";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("insert lines via apply_patch").await?;

    assert_eq!(
        harness.read_file_text("insert_only.txt").await?,
        "alpha\nbeta\nomega\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_move_overwrites_existing_destination() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("old/name.txt", "from\n").await?;
    harness
        .write_file("renamed/dir/name.txt", "existing\n")
        .await?;

    let patch = "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-from\n+new\n*** End Patch";
    let call_id = "apply-move-overwrite";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply move overwrite patch").await?;

    assert!(!harness.path_exists("old/name.txt").await?);
    assert_eq!(
        harness.read_file_text("renamed/dir/name.txt").await?,
        "new\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_move_without_content_change_has_no_turn_diff() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    let test = harness.test();
    let codex = test.codex.clone();

    harness.write_file("old/name.txt", "same\n").await?;

    let patch = "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/name.txt\n@@\n same\n*** End Patch";
    let call_id = "apply-move-no-change";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    submit_without_wait(&harness, "rename without content change").await?;

    let mut saw_turn_diff = false;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(_) => {
            saw_turn_diff = true;
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert!(!saw_turn_diff, "pure rename should not emit a turn diff");
    assert!(!harness.path_exists("old/name.txt").await?);
    assert_eq!(harness.read_file_text("renamed/name.txt").await?, "same\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_add_overwrites_existing_file() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("duplicate.txt", "old content\n").await?;

    let patch = "*** Begin Patch\n*** Add File: duplicate.txt\n+new content\n*** End Patch";
    let call_id = "apply-add-overwrite";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply add overwrite patch").await?;

    assert_eq!(
        harness.read_file_text("duplicate.txt").await?,
        "new content\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_rejects_invalid_hunk_header() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    let patch = "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch";
    let call_id = "apply-invalid-header";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply invalid header patch").await?;

    let out = harness.apply_patch_output(call_id).await;

    assert!(
        out.contains("apply_patch verification failed"),
        "expected verification failure message"
    );
    assert!(
        out.contains("is not a valid hunk header"),
        "expected parse diagnostics in output: {out:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_reports_missing_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("modify.txt", "line1\nline2\n").await?;

    let patch =
        "*** Begin Patch\n*** Update File: modify.txt\n@@\n-missing\n+changed\n*** End Patch";
    let call_id = "apply-missing-context";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply missing context patch").await?;

    let out = harness.apply_patch_output(call_id).await;

    assert!(
        out.contains("apply_patch verification failed"),
        "expected verification failure message"
    );
    assert!(out.contains("Failed to find expected lines in"));
    assert_eq!(
        harness.read_file_text("modify.txt").await?,
        "line1\nline2\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_reports_missing_target_file() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    let patch = "*** Begin Patch\n*** Update File: missing.txt\n@@\n-nope\n+better\n*** End Patch";
    let call_id = "apply-missing-file";
    mount_apply_patch(&harness, call_id, patch, "fail").await;

    harness.submit("attempt to update a missing file").await?;

    let out = harness.apply_patch_output(call_id).await;
    assert!(
        out.contains("apply_patch verification failed"),
        "expected verification failure message"
    );
    assert!(
        out.contains("Failed to read file to update"),
        "expected missing file diagnostics: {out}"
    );
    assert!(
        out.contains("missing.txt"),
        "expected missing file path in diagnostics: {out}"
    );
    assert!(!harness.path_exists("missing.txt").await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_delete_missing_file_reports_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    let patch = "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch";
    let call_id = "apply-delete-missing";
    mount_apply_patch(&harness, call_id, patch, "fail").await;

    harness.submit("attempt to delete missing file").await?;

    let out = harness.apply_patch_output(call_id).await;

    assert!(
        out.contains("apply_patch verification failed"),
        "expected verification failure message: {out}"
    );
    assert!(
        out.contains("Failed to read"),
        "missing delete diagnostics should mention read failure: {out}"
    );
    assert!(
        out.contains("missing.txt"),
        "missing delete diagnostics should surface target path: {out}"
    );
    assert!(!harness.path_exists("missing.txt").await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_rejects_empty_patch() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    let patch = "*** Begin Patch\n*** End Patch";
    let call_id = "apply-empty";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply empty patch").await?;

    let out = harness.apply_patch_output(call_id).await;
    assert!(
        out.contains("No files were modified."),
        "expected rejection for empty patch: {out}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_delete_directory_reports_verification_error() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.create_dir_all("dir").await?;

    let patch = "*** Begin Patch\n*** Delete File: dir\n*** End Patch";
    let call_id = "apply-delete-dir";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("delete a directory via apply_patch").await?;

    let out = harness.apply_patch_output(call_id).await;
    assert!(out.contains("apply_patch verification failed"));
    assert!(out.contains("Failed to read"));
    Ok(())
}

/// An intercepted update can verify absolute paths after the turn cwd disappears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intercepted_apply_patch_updates_absolute_target_after_turn_cwd_is_removed() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| builder.with_model("gpt-5.5")).await?;
    let test = harness.test();
    let workspace = test.workspace_path_uri("")?;
    let original_cwd = test.workspace_path_uri("policy-cwd")?;
    let allowed_target = test.workspace_path_uri("allowed.txt")?;
    harness.create_dir_all("policy-cwd").await?;
    harness.write_file("allowed.txt", "original\n").await?;

    let selection = TurnEnvironmentSelection {
        cwd: original_cwd,
        workspace_roots: vec![workspace.clone()],
        ..test.executor_environment().selection().clone()
    };

    let workdir = workspace.inferred_native_path_string();
    let allowed_patch = format!(
        "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: {}\n@@\n-original\n+changed\n*** End Patch\nEOF\n",
        allowed_target.inferred_native_path_string(),
    );
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-remove"),
                ev_exec_command_call_with_args(
                    "remove-cwd",
                    &json!({"cmd": "rmdir policy-cwd", "workdir": workdir, "login": false}),
                ),
                ev_completed("resp-remove"),
            ]),
            sse(vec![
                ev_response_created("resp-allowed"),
                ev_exec_command_call_with_args(
                    "patch-allowed",
                    &json!({"cmd": allowed_patch, "workdir": workdir, "login": false}),
                ),
                ev_completed("resp-allowed"),
            ]),
            sse(vec![
                ev_assistant_message("msg-done", "done"),
                ev_completed("resp-done"),
            ]),
        ],
    )
    .await;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "remove the turn cwd, then update the file".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    vec![selection.into_request()],
                )),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert!(!harness.path_exists("policy-cwd").await?);
    let allowed_output = responses
        .function_call_output_text("patch-allowed")
        .expect("the model should receive the allowed patch output");
    assert!(allowed_output.contains("Success."), "{allowed_output}");
    assert_eq!(harness.read_file_text("allowed.txt").await?, "changed\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_verification_failure_has_no_side_effects() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    // Compose a patch that would create a file, then fail verification on an update.
    let call_id = "apply-partial-no-side-effects";
    let patch = "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch";

    mount_apply_patch(&harness, call_id, patch, "failed").await;

    harness.submit("attempt partial apply patch").await?;

    assert!(
        !harness.path_exists("created.txt").await?,
        "verification failure should prevent any filesystem changes"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_exec_command_heredoc_with_cd_updates_relative_workdir() -> Result<()> {
    // TODO(anp): Remove after apply_patch shell fixtures use target-native commands.
    skip_if_wine_exec!(Ok(()), "uses a POSIX shell heredoc and cd command");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| builder.with_model("gpt-5.5")).await?;

    // Prepare a file inside a subdir; update it via cd && apply_patch heredoc form.
    harness.write_file("sub/in_sub.txt", "before\n").await?;

    let script = "cd sub && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: in_sub.txt\n@@\n-before\n+after\n*** End Patch\nEOF\n";
    let call_id = "shell-heredoc-cd";
    let bodies = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_exec_command_call(call_id, script),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "ok"),
            ev_completed("resp-2"),
        ]),
    ];
    mount_sse_sequence(harness.server(), bodies).await;

    harness.submit("apply via shell heredoc with cd").await?;

    let out = harness.function_call_stdout(call_id).await;
    assert!(
        out.contains("Success."),
        "expected successful apply_patch invocation via exec_command: {out}"
    );
    assert_eq!(harness.read_file_text("sub/in_sub.txt").await?, "after\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_can_use_exec_command_output_as_patch_input() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(
        Ok(()),
        "exec_command output producer runs in the test runner, not in the remote apply_patch workspace",
    );

    let harness =
        apply_patch_harness_with(|builder| builder.with_model("gpt-5.5").with_windows_cmd_shell())
            .await?;

    let source_contents = "line1\nnaïve café\nline3\n";
    harness.write_file("source.txt", source_contents).await?;

    let read_call_id = "read-source";
    let apply_call_id = "apply-from-read";

    fn stdout_from_shell_output(output: &str) -> String {
        let normalized = output.replace("\r\n", "\n").replace('\r', "\n");
        normalized
            .split_once("Output:\n")
            .map(|x| x.1)
            .unwrap_or("")
            .trim_end_matches('\n')
            .to_string()
    }

    fn function_call_output_text(body: &serde_json::Value, call_id: &str) -> String {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("type").and_then(serde_json::Value::as_str)
                        == Some("function_call_output")
                        && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
                })
            })
            .and_then(|item| item.get("output").and_then(serde_json::Value::as_str))
            .expect("function_call_output output string")
            .to_string()
    }

    struct DynamicApplyFromRead {
        num_calls: AtomicI32,
        read_call_id: String,
        apply_call_id: String,
    }

    impl Respond for DynamicApplyFromRead {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let call_num = self.num_calls.fetch_add(1, Ordering::SeqCst);
            match call_num {
                0 => {
                    let command = if cfg!(windows) {
                        // Encode the nested PowerShell script so `cmd.exe /c` does not leave the
                        // read command wrapped in quotes, and suppress progress records so the
                        // shell tool only returns the file contents back to apply_patch.
                        let script = "$ProgressPreference = 'SilentlyContinue'; [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [System.IO.File]::ReadAllText('source.txt', [System.Text.UTF8Encoding]::new($false))";
                        let encoded = BASE64_STANDARD.encode(
                            script
                                .encode_utf16()
                                .flat_map(u16::to_le_bytes)
                                .collect::<Vec<u8>>(),
                        );
                        format!(
                            "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}"
                        )
                    } else {
                        "cat source.txt".to_string()
                    };
                    let args = json!({
                        "cmd": command,
                        "login": false,
                    });
                    let body = sse(vec![
                        ev_response_created("resp-1"),
                        ev_exec_command_call_with_args(&self.read_call_id, &args),
                        ev_completed("resp-1"),
                    ]);
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body)
                }
                1 => {
                    let body_json: serde_json::Value =
                        request.body_json().expect("request body should be json");
                    let read_output = function_call_output_text(&body_json, &self.read_call_id);
                    let stdout = stdout_from_shell_output(&read_output);
                    let patch_lines = stdout
                        .lines()
                        .map(|line| format!("+{line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let patch = format!(
                        "*** Begin Patch\n*** Add File: target.txt\n{patch_lines}\n*** End Patch"
                    );

                    let body = sse(vec![
                        ev_response_created("resp-2"),
                        ev_apply_patch_custom_tool_call(&self.apply_call_id, &patch),
                        ev_completed("resp-2"),
                    ]);
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body)
                }
                2 => {
                    let body = sse(vec![
                        ev_assistant_message("msg-1", "ok"),
                        ev_completed("resp-3"),
                    ]);
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body)
                }
                _ => panic!("no response for call {call_num}"),
            }
        }
    }

    let responder = DynamicApplyFromRead {
        num_calls: AtomicI32::new(0),
        read_call_id: read_call_id.to_string(),
        apply_call_id: apply_call_id.to_string(),
    };
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(responder)
        .expect(3)
        .mount(harness.server())
        .await;

    harness
        .submit("read source.txt, then apply it to target.txt")
        .await?;

    let target_contents = harness.read_file_text("target.txt").await?;
    assert_eq!(target_contents, source_contents);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_custom_tool_streaming_emits_updated_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| {
        builder.with_config(|config| {
            config.features.enable(Feature::ApplyPatchStreamingEvents);
        })
    })
    .await?;
    let test = harness.test();
    let codex = test.codex.clone();
    let call_id = "apply-patch-streaming";
    let patch = "*** Begin Patch\n*** Add File: streamed.txt\n+hello\n+world\n*** End Patch";
    mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                json!({
                    "type": "response.output_item.added",
                    "item": {
                        "type": "custom_tool_call",
                        "call_id": call_id,
                        "name": "apply_patch",
                        "input": "",
                    }
                }),
                json!({
                    "type": "response.custom_tool_call_input.delta",
                    "call_id": call_id,
                    "delta": "*** Begin Patch\n",
                }),
                json!({
                    "type": "response.custom_tool_call_input.delta",
                    "call_id": call_id,
                    "delta": "*** Add File: streamed.txt\n+hello",
                }),
                json!({
                    "type": "response.custom_tool_call_input.delta",
                    "call_id": call_id,
                    "delta": "\n+world\n*** End Patch",
                }),
                ev_apply_patch_custom_tool_call(call_id, patch),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    submit_without_wait(&harness, "create streamed file").await?;

    let mut updates = Vec::new();
    wait_for_event(&codex, |event| match event {
        EventMsg::PatchApplyUpdated(update) => {
            updates.push(update.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert_eq!(
        updates
            .iter()
            .map(|update| update.call_id.as_str())
            .collect::<Vec<_>>(),
        vec![call_id, call_id]
    );
    assert_eq!(
        updates
            .first()
            .expect("first update")
            .changes
            .get(&std::path::PathBuf::from("streamed.txt")),
        Some(&codex_protocol::protocol::FileChange::Add {
            content: String::new(),
        })
    );
    assert_eq!(
        updates
            .last()
            .expect("last update")
            .changes
            .get(&std::path::PathBuf::from("streamed.txt")),
        Some(&codex_protocol::protocol::FileChange::Add {
            content: "hello\nworld\n".to_string(),
        })
    );
    assert_eq!(
        harness.read_file_text("streamed.txt").await?,
        "hello\nworld\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_exec_command_heredoc_with_cd_emits_turn_diff() -> Result<()> {
    // TODO(anp): Remove after apply_patch shell fixtures use target-native commands.
    skip_if_wine_exec!(Ok(()), "uses a POSIX shell heredoc and cd command");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| builder.with_model("gpt-5.5")).await?;
    let test = harness.test();
    let codex = test.codex.clone();

    // Prepare a file inside a subdir; update it via cd && apply_patch heredoc form.
    harness.write_file("sub/in_sub.txt", "before\n").await?;

    let script = "cd sub && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: in_sub.txt\n@@\n-before\n+after\n*** End Patch\nEOF\n";
    let call_id = "shell-heredoc-cd";
    let args = json!({ "cmd": script, "yield_time_ms": 30_000 });
    let bodies = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "ok"),
            ev_completed("resp-2"),
        ]),
    ];
    mount_sse_sequence(harness.server(), bodies).await;

    submit_without_wait(&harness, "apply via shell heredoc with cd").await?;

    let mut saw_turn_diff = None;
    let mut saw_patch_begin = false;
    let mut patch_end_success = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::PatchApplyBegin(begin) => {
            saw_patch_begin = true;
            assert_eq!(begin.call_id, call_id);
            false
        }
        EventMsg::PatchApplyEnd(end) => {
            assert_eq!(end.call_id, call_id);
            patch_end_success = Some(end.success);
            false
        }
        EventMsg::TurnDiff(ev) => {
            saw_turn_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert!(saw_patch_begin, "expected PatchApplyBegin event");
    let patch_end_success =
        patch_end_success.expect("expected PatchApplyEnd event to capture success flag");
    assert!(patch_end_success);

    let diff = saw_turn_diff.expect("expected TurnDiff event");
    assert!(diff.contains("diff --git"), "diff header missing: {diff:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_turn_diff_paths_stay_repo_relative_when_session_cwd_is_nested() -> Result<()> {
    // TODO(anp): Remove after apply_patch diff fixtures use target-native paths.
    skip_if_wine_exec!(Ok(()), "asserts POSIX repository paths");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| {
        builder
            .with_model("gpt-5.5")
            .with_config(|config| {
                config.cwd = config.cwd.join("subdir");
            })
            .with_workspace_setup(|cwd, fs| async move {
                let cwd_uri = PathUri::from_host_native_path(&cwd)?;
                fs.create_directory(
                    &cwd_uri,
                    CreateDirectoryOptions {
                        recursive: true,
                        follow_symlinks: true,
                    },
                )
                .await?;
                let repo_root = cwd.parent().expect("nested cwd should have parent");
                let git_uri = PathUri::from_host_native_path(repo_root.join(".git"))?;
                let repo_file_uri = PathUri::from_host_native_path(repo_root.join("repo.txt"))?;
                fs.write_file(
                    &git_uri,
                    b"gitdir: /tmp/fake-worktree\n".to_vec(),
                    Default::default(),
                )
                .await?;
                fs.write_file(&repo_file_uri, b"before\n".to_vec(), Default::default())
                    .await?;
                Ok(())
            })
    })
    .await?;
    let test = harness.test();
    let codex = test.codex.clone();
    let repo_root = harness
        .test()
        .config
        .cwd
        .parent()
        .expect("nested cwd should have parent");

    let call_id = "apply-nested-cwd-repo-relative";
    let patch = "*** Begin Patch\n*** Update File: ../repo.txt\n@@\n-before\n+after\n*** End Patch";
    mount_apply_patch(&harness, call_id, patch, "updated repo-relative path").await;

    submit_without_wait(&harness, "update file outside nested cwd but inside repo").await?;

    let mut last_diff: Option<String> = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(ev) => {
            last_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    let diff = last_diff.expect("expected TurnDiff event after update");
    assert!(
        diff.contains("diff --git a/repo.txt b/repo.txt"),
        "diff should stay repo-relative: {diff:?}"
    );
    assert!(
        !diff.contains(repo_root.as_path().to_string_lossy().as_ref()),
        "diff should not leak absolute repo paths: {diff:?}"
    );
    Ok(())
}

#[test_case("codex_work_web", true, "work.txt"; "web work uses cwd when enabled")]
#[test_case("codex_work_mobile", true, "work.txt"; "mobile work uses cwd when enabled")]
#[test_case("codex_work_web", false, "subdir/work.txt"; "disabled feature keeps repository root")]
#[test_case("codex_work_desktop", true, "work.txt"; "desktop uses cwd when enabled")]
#[test_case("codex_work_cca", true, "work.txt"; "legacy cca uses cwd when enabled")]
#[test_case("codex_cli_rs", true, "work.txt"; "coding originator uses cwd when enabled")]
#[test_case("codex_cli_rs", false, "subdir/work.txt"; "coding originator keeps repository root when disabled")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_turn_diff_skips_git_root_when_feature_is_enabled(
    originator: &str,
    cwd_relative_turn_diffs: bool,
    expected_path: &str,
) -> Result<()> {
    skip_if_wine_exec!(Ok(()), "asserts POSIX repository paths");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| {
        builder
            .with_model("gpt-5.5")
            .with_config(move |config| {
                config.cwd = config.cwd.join("subdir");
                if cwd_relative_turn_diffs {
                    config.features.enable(Feature::CwdRelativeTurnDiffs);
                }
            })
            .with_workspace_setup(|cwd, fs| async move {
                let cwd_uri = PathUri::from_host_native_path(&cwd)?;
                fs.create_directory(
                    &cwd_uri,
                    CreateDirectoryOptions {
                        recursive: true,
                        follow_symlinks: true,
                    },
                )
                .await?;
                let repo_root = cwd.parent().expect("nested cwd should have parent");
                let git_uri = PathUri::from_host_native_path(repo_root.join(".git"))?;
                let work_file_uri = PathUri::from_host_native_path(cwd.join("work.txt"))?;
                fs.write_file(
                    &git_uri,
                    b"gitdir: /tmp/fake-worktree\n".to_vec(),
                    Default::default(),
                )
                .await?;
                fs.write_file(&work_file_uri, b"before\n".to_vec(), Default::default())
                    .await?;
                Ok(())
            })
    })
    .await?;
    let test = harness.test();
    let codex = test
        .thread_manager
        .start_thread(StartThreadOptions {
            metrics_service_name: Some(originator.to_string()),
            ..test.start_thread_options().await
        })
        .await?
        .thread;
    let patch = "*** Begin Patch\n*** Update File: work.txt\n@@\n-before\n+after\n*** End Patch";
    mount_apply_patch(&harness, "apply-work-diff-root", patch, "updated work file").await;

    codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "update the work file".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                ..Default::default()
            }),
        )
        .await?;

    let mut last_diff = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(event) => {
            last_diff = Some(event.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    let diff = last_diff.expect("expected TurnDiff event after update");
    assert!(
        diff.contains(&format!("diff --git a/{expected_path} b/{expected_path}")),
        "unexpected turn diff path for {originator}: {diff:?}",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_exec_command_failure_propagates_error_and_skips_diff() -> Result<()> {
    // TODO(anp): Remove after apply_patch shell fixtures use target-native commands.
    skip_if_wine_exec!(Ok(()), "uses a POSIX shell heredoc");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| builder.with_model("gpt-5.5")).await?;
    let test = harness.test();
    let codex = test.codex.clone();

    harness.write_file("invalid.txt", "ok\n").await?;

    let script = "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: invalid.txt\n@@\n-nope\n+changed\n*** End Patch\nEOF\n";
    let call_id = "shell-apply-failure";
    let args = json!({ "cmd": script, "yield_time_ms": 5_000 });
    let bodies = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "fail"),
            ev_completed("resp-2"),
        ]),
    ];
    mount_sse_sequence(harness.server(), bodies).await;

    submit_without_wait(&harness, "apply patch via shell").await?;

    let mut saw_turn_diff = false;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(_) => {
            saw_turn_diff = true;
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert!(
        !saw_turn_diff,
        "turn diff should not be emitted when shell apply_patch fails verification"
    );

    let out = harness.function_call_stdout(call_id).await;
    assert!(
        out.contains("Failed to find expected lines in"),
        "expected failure diagnostics: {out}"
    );
    assert!(
        out.contains("invalid.txt"),
        "expected file path in output: {out}"
    );
    assert_eq!(harness.read_file_text("invalid.txt").await?, "ok\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_shell_accepts_lenient_heredoc_wrapped_patch() -> Result<()> {
    // TODO(anp): Remove after apply_patch shell fixtures use target-native commands.
    skip_if_wine_exec!(Ok(()), "uses a POSIX shell heredoc");
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    let file_name = "lenient.txt";
    let patch_inner =
        format!("*** Begin Patch\n*** Add File: {file_name}\n+lenient\n*** End Patch\n");
    let call_id = "apply-lenient";
    mount_apply_patch_model_output(
        &harness,
        call_id,
        patch_inner.as_str(),
        "ok",
        ApplyPatchModelOutput::ExecCommandViaHeredoc,
    )
    .await;

    harness.submit("apply lenient heredoc patch").await?;

    let out = harness.function_call_stdout(call_id).await;
    assert!(
        serde_json::from_str::<serde_json::Value>(&out).is_err(),
        "expected heredoc apply_patch output to be plain text"
    );
    assert!(
        out.contains("Success. Updated the following files:"),
        "expected successful apply_patch output: {out}"
    );
    assert!(
        out.contains(&format!("A {file_name}")),
        "expected created file in apply_patch output: {out}"
    );
    assert_eq!(harness.read_file_text(file_name).await?, "lenient\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_end_of_file_anchor() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("tail.txt", "alpha\nlast\n").await?;

    let patch = "*** Begin Patch\n*** Update File: tail.txt\n@@\n-last\n+end\n*** End of File\n*** End Patch";
    let call_id = "apply-eof";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply EOF-anchored patch").await?;
    assert_eq!(harness.read_file_text("tail.txt").await?, "alpha\nend\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_cli_missing_second_chunk_context_rejected() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness.write_file("two_chunks.txt", "a\nb\nc\nd\n").await?;

    // First chunk has @@, second chunk intentionally omits @@ to trigger parse error.
    let patch =
        "*** Begin Patch\n*** Update File: two_chunks.txt\n@@\n-b\n+B\n\n-d\n+D\n*** End Patch";
    let call_id = "apply-missing-ctx-2nd";
    mount_apply_patch(&harness, call_id, patch, "fail").await;

    harness.submit("apply missing context second chunk").await?;

    let out = harness.apply_patch_output(call_id).await;
    assert!(out.contains("apply_patch verification failed"));
    assert!(
        out.contains("Failed to find expected lines in"),
        "expected hunk context diagnostics: {out}"
    );
    // Original file unchanged on failure
    assert_eq!(
        harness.read_file_text("two_chunks.txt").await?,
        "a\nb\nc\nd\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_emits_turn_diff_event_with_unified_diff() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    let test = harness.test();
    let codex = test.codex.clone();

    let call_id = "apply-diff-event";
    let file = "udiff.txt";
    let patch = format!("*** Begin Patch\n*** Add File: {file}\n+hello\n*** End Patch\n");
    mount_apply_patch(&harness, call_id, patch.as_str(), "ok").await;

    submit_without_wait(&harness, "emit diff").await?;

    let mut saw_turn_diff = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(ev) => {
            saw_turn_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    let diff = saw_turn_diff.expect("expected TurnDiff event");
    // Basic markers of a unified diff with file addition
    assert!(diff.contains("diff --git"), "diff header missing: {diff:?}");
    assert!(diff.contains("--- /dev/null") || diff.contains("--- a/"));
    assert!(diff.contains("+++ b/"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_turn_diff_emits_portable_paths_for_remote_cwd() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let harness = apply_patch_harness().await?;
    let test = harness.test();
    let codex = test.codex.clone();

    let call_id = "apply-foreign-windows-diff";
    let file = "nested/foreign.txt";
    let patch = format!("*** Begin Patch\n*** Add File: {file}\n+hello\n*** End Patch");
    mount_apply_patch(&harness, call_id, &patch, "ok").await;

    submit_without_wait(&harness, "emit diff for a foreign Windows cwd").await?;

    let mut last_diff = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(ev) => {
            last_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    let cwd = &test.executor_environment().selection().cwd;
    let file_uri = cwd.join(file)?;
    let expected_relative_path = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "nested/foreign.txt",
        TestTargetOs::Windows => r"nested\foreign.txt",
    };
    assert_eq!(
        file_uri.relative_path_from(cwd).as_deref(),
        Some(expected_relative_path)
    );
    assert_eq!(
        test.fs()
            .read_file_text(&file_uri, Default::default())
            .await?,
        "hello\n"
    );
    assert_eq!(
        last_diff.expect("expected TurnDiff event"),
        r#"diff --git a/nested/foreign.txt b/nested/foreign.txt
new file mode 100644
index 0000000000000000000000000000000000000000..ce013625030ba8dba906f756967f9e9ca394464a
--- /dev/null
+++ b/nested/foreign.txt
@@ -0,0 +1 @@
+hello
"#
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_turn_diff_tracks_local_and_remote_environment_paths() -> Result<()> {
    // TODO(anp): Remove after shared-cwd helpers use target-native paths.
    skip_if_target_windows!(
        Ok(()),
        "requires a cwd valid in local POSIX and remote Windows environments"
    );
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let file_name = "shared-turn-diff.txt";
    let shared_cwd = PathBuf::from(format!(
        "/tmp/codex-remote-turn-diff-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ))
    .abs();
    let shared_cwd_uri = PathUri::from_host_native_path(&shared_cwd)?;
    let _ = fs::remove_dir_all(shared_cwd.as_path());
    test.fs()
        .remove(
            &shared_cwd_uri,
            RemoveOptions {
                recursive: true,
                force: true,
                follow_symlinks: true,
            },
        )
        .await?;
    fs::create_dir_all(shared_cwd.as_path())?;
    test.fs()
        .create_directory(
            &shared_cwd_uri,
            CreateDirectoryOptions {
                recursive: true,
                follow_symlinks: true,
            },
        )
        .await?;

    let local_patch = format!(
        "*** Begin Patch\n*** Environment ID: {LOCAL_ENVIRONMENT_ID}\n*** Add File: {file_name}\n+local\n*** End Patch"
    );
    let remote_patch = format!(
        "*** Begin Patch\n*** Environment ID: {REMOTE_ENVIRONMENT_ID}\n*** Add File: {file_name}\n+remote\n*** End Patch"
    );
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-local"),
                ev_apply_patch_custom_tool_call("call-local", &local_patch),
                ev_completed("resp-local"),
            ]),
            sse(vec![
                ev_response_created("resp-remote"),
                ev_apply_patch_custom_tool_call("call-remote", &remote_patch),
                ev_completed("resp-remote"),
            ]),
            sse(vec![
                ev_response_created("resp-done"),
                ev_assistant_message("msg-done", "done"),
                ev_completed("resp-done"),
            ]),
        ],
    )
    .await;

    let environments = vec![
        local(shared_cwd.clone()),
        TurnEnvironmentSelection {
            selected_capability_roots: Default::default(),
            environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&shared_cwd),
            workspace_roots: vec![PathUri::from_abs_path(&shared_cwd)],
            config: EnvironmentConfigState::FromThread,
        },
    ];
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "apply matching patches to local and remote environments".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(codex_protocol::protocol::TurnEnvironmentRequests::new(
                    test.config.cwd.clone(),
                    environments
                        .into_iter()
                        .map(TurnEnvironmentSelection::into_request)
                        .collect(),
                )),
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

    let mut last_diff = None;
    wait_for_event(&test.codex, |event| match event {
        EventMsg::TurnDiff(ev) => {
            last_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    assert_eq!(fs::read_to_string(shared_cwd.join(file_name))?, "local\n");
    assert_eq!(
        test.fs()
            .read_file_text(
                &PathUri::from_host_native_path(shared_cwd.join(file_name))?,
                Default::default()
            )
            .await?,
        "remote\n"
    );
    let diff = last_diff.expect("expected TurnDiff event");
    assert_eq!(
        diff,
        r#"diff --git a/local/shared-turn-diff.txt b/local/shared-turn-diff.txt
new file mode 100644
index 0000000000000000000000000000000000000000..40830374235df1c19661a2901b7ca73cc9499f3d
--- /dev/null
+++ b/local/shared-turn-diff.txt
@@ -0,0 +1 @@
+local
diff --git a/remote/shared-turn-diff.txt b/remote/shared-turn-diff.txt
new file mode 100644
index 0000000000000000000000000000000000000000..9c998f7b995a7327177b38a90d1385170df2b94b
--- /dev/null
+++ b/remote/shared-turn-diff.txt
@@ -0,0 +1 @@
+remote
"#
    );

    let _ = fs::remove_dir_all(shared_cwd.as_path());
    test.fs()
        .remove(
            &shared_cwd_uri,
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
async fn apply_patch_aggregates_diff_across_multiple_tool_calls() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    let test = harness.test();
    let codex = test.codex.clone();

    let call1 = "agg-1";
    let call2 = "agg-2";
    let patch1 = "*** Begin Patch\n*** Add File: agg/a.txt\n+v1\n*** End Patch";
    let patch2 = "*** Begin Patch\n*** Update File: agg/a.txt\n@@\n-v1\n+v2\n*** Add File: agg/b.txt\n+B\n*** End Patch";

    let s1 = sse(vec![
        ev_response_created("resp-1"),
        ev_apply_patch_custom_tool_call(call1, patch1),
        ev_completed("resp-1"),
    ]);
    let s2 = sse(vec![
        ev_response_created("resp-2"),
        ev_apply_patch_custom_tool_call(call2, patch2),
        ev_completed("resp-2"),
    ]);
    let s3 = sse(vec![
        ev_assistant_message("msg-1", "ok"),
        ev_completed("resp-3"),
    ]);
    mount_sse_sequence(harness.server(), vec![s1, s2, s3]).await;

    submit_without_wait(&harness, "aggregate diffs").await?;

    let mut last_diff: Option<String> = None;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnDiff(ev) => {
            last_diff = Some(ev.unified_diff.clone());
            false
        }
        EventMsg::TurnComplete(_) => true,
        _ => false,
    })
    .await;

    let diff = last_diff.expect("expected TurnDiff after two patches");
    assert!(diff.contains("agg/a.txt"), "diff missing agg/a.txt: {diff}");
    assert!(diff.contains("agg/b.txt"), "diff missing agg/b.txt: {diff}");
    // Final content reflects v2 for a.txt
    assert!(diff.contains("+v2\n") || diff.contains("v2\n"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_aggregates_diff_preserves_success_after_failure() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;
    let test = harness.test();
    let codex = test.codex.clone();

    let call_success = "agg-success";
    let call_failure = "agg-failure";
    let patch_success = "*** Begin Patch\n*** Add File: partial/success.txt\n+ok\n*** End Patch";
    let patch_failure =
        "*** Begin Patch\n*** Update File: partial/success.txt\n@@\n-missing\n+new\n*** End Patch";

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_apply_patch_custom_tool_call(call_success, patch_success),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_apply_patch_custom_tool_call(call_failure, patch_failure),
            ev_completed("resp-2"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "failed"),
            ev_completed("resp-3"),
        ]),
    ];
    mount_sse_sequence(harness.server(), responses).await;

    submit_without_wait(&harness, "apply patch twice with failure").await?;

    let mut last_diff: Option<String> = None;
    wait_for_event_with_timeout(
        &codex,
        |event| match event {
            EventMsg::TurnDiff(ev) => {
                last_diff = Some(ev.unified_diff.clone());
                false
            }
            EventMsg::TurnComplete(_) => true,
            _ => false,
        },
        Duration::from_secs(30),
    )
    .await;

    let diff = last_diff.expect("expected TurnDiff after failed patch");
    assert!(
        diff.contains("partial/success.txt"),
        "diff should still include the successful addition: {diff}"
    );
    assert!(
        diff.contains("+ok"),
        "diff should include contents from successful patch: {diff}"
    );

    let failure_out = harness.custom_tool_call_output(call_failure).await;
    assert!(
        failure_out.contains("apply_patch verification failed"),
        "expected verification failure output: {failure_out}"
    );
    assert!(
        failure_out.contains("Failed to find expected lines in"),
        "expected missing context diagnostics: {failure_out}"
    );

    assert_eq!(harness.read_file_text("partial/success.txt").await?, "ok\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_clears_aggregated_diff_after_inexact_delta() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness_with(|builder| {
        builder.with_workspace_setup(|cwd, fs| async move {
            let binary_path_uri = executor_path_uri(cwd.join("binary.dat"))?;
            fs.write_file(&binary_path_uri, vec![0xff, 0xfe, 0xfd], Default::default())
                .await?;
            Ok(())
        })
    })
    .await?;
    let test = harness.test();
    let codex = test.codex.clone();

    let call_success = "agg-success";
    let call_inexact = "agg-inexact";
    let patch_success = "*** Begin Patch\n*** Add File: partial/success.txt\n+ok\n*** End Patch";
    let patch_inexact = "*** Begin Patch\n*** Add File: binary.dat\n+text\n*** End Patch";

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_apply_patch_custom_tool_call(call_success, patch_success),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_apply_patch_custom_tool_call(call_inexact, patch_inexact),
            ev_completed("resp-2"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-3"),
        ]),
    ];
    mount_sse_sequence(harness.server(), responses).await;

    submit_without_wait(&harness, "apply patch twice with inexact delta").await?;

    let mut last_diff: Option<String> = None;
    wait_for_event_with_timeout(
        &codex,
        |event| match event {
            EventMsg::TurnDiff(ev) => {
                last_diff = Some(ev.unified_diff.clone());
                false
            }
            EventMsg::TurnComplete(_) => true,
            _ => false,
        },
        Duration::from_secs(30),
    )
    .await;

    assert_eq!(
        last_diff.as_deref(),
        Some(""),
        "inexact delta should clear the aggregate diff"
    );
    assert_eq!(harness.read_file_text("partial/success.txt").await?, "ok\n");
    assert_eq!(harness.read_file_text("binary.dat").await?, "text\n");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_patch_change_context_disambiguates_target() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = apply_patch_harness().await?;

    harness
        .write_file("multi_ctx.txt", "fn a\nx=10\ny=2\nfn b\nx=10\ny=20\n")
        .await?;

    let patch =
        "*** Begin Patch\n*** Update File: multi_ctx.txt\n@@ fn b\n-x=10\n+x=11\n*** End Patch";
    let call_id = "apply-ctx";
    mount_apply_patch(&harness, call_id, patch, "ok").await;

    harness.submit("apply with change_context").await?;

    let contents = harness.read_file_text("multi_ctx.txt").await?;
    assert_eq!(contents, "fn a\nx=10\ny=2\nfn b\nx=11\ny=20\n");
    Ok(())
}

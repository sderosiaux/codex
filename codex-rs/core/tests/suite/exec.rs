#![cfg(unix)]

use codex_core::exec::ExecCapturePolicy;
use codex_core::exec::ExecParams;
use codex_core::exec::process_exec_tool_call;
use codex_protocol::error::Result;
use codex_protocol::exec_output::ExecToolCallOutput;
use core_test_support::PathExt;
use std::collections::HashMap;
use tempfile::TempDir;

async fn run_test_cmd<I, S>(tmp: TempDir, command: I) -> Result<ExecToolCallOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let params = ExecParams {
        command: command.into_iter().map(Into::into).collect(),
        cwd: tmp.path().abs(),
        expiration: 1000.into(),
        capture_policy: ExecCapturePolicy::ShellTool,
        env: HashMap::new(),
    };

    process_exec_tool_call(params, /*stdout_stream*/ None).await
}

/// Command succeeds with exit code 0 normally
#[tokio::test]
async fn exit_code_0_succeeds() {
    let tmp = TempDir::new().expect("should be able to create temp dir");
    let cmd = vec!["echo", "hello"];

    let output = run_test_cmd(tmp, cmd).await.unwrap();
    assert_eq!(output.stdout.text, "hello\n");
    assert_eq!(output.stderr.text, "");
    assert_eq!(output.stdout.truncated_after_lines, None);
}

/// Command succeeds with exit code 0 normally
#[tokio::test]
async fn truncates_output_lines() {
    let tmp = TempDir::new().expect("should be able to create temp dir");
    let cmd = vec!["seq", "300"];

    let output = run_test_cmd(tmp, cmd).await.unwrap();

    let expected_output = (1..=300)
        .map(|i| format!("{i}\n"))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(output.stdout.text, expected_output);
    assert_eq!(output.stdout.truncated_after_lines, None);
}

/// Command succeeds with exit code 0 normally
#[tokio::test]
async fn truncates_output_bytes() {
    let tmp = TempDir::new().expect("should be able to create temp dir");
    // each line is 1000 bytes
    let cmd = vec!["bash", "-lc", "seq 15 | awk '{printf \"%-1000s\\n\", $0}'"];

    let output = run_test_cmd(tmp, cmd).await.unwrap();

    assert!(output.stdout.text.len() >= 15000);
    assert_eq!(output.stdout.truncated_after_lines, None);
}

/// Command not found returns exit code 127 as a normal result, not an error
#[tokio::test]
async fn exit_command_not_found_is_ok() {
    let tmp = TempDir::new().expect("should be able to create temp dir");
    let cmd = vec!["/bin/bash", "-c", "nonexistent_command_12345"];
    run_test_cmd(tmp, cmd).await.unwrap();
}

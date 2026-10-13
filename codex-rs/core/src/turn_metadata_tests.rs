use super::*;

use crate::responses_metadata::CONTEXT_WINDOW_ID_KEY;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionImplementation;
use crate::responses_metadata::CompactionPhase;
use crate::responses_metadata::CompactionReason;
use crate::responses_metadata::CompactionTrigger;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_metadata::FORKED_FROM_ORDINAL_EXCLUSIVE_KEY;
use crate::responses_metadata::INSTALLATION_ID_KEY;
use crate::responses_metadata::LEGACY_CODE_MODE_TOOL_NAMES_KEY;
use crate::responses_metadata::MCP_ATTRIBUTION_CLIENT_METADATA_KEY;
use crate::responses_metadata::PARENT_TURN_ID_KEY;
use crate::responses_metadata::ROOT_TURN_ID_KEY;
use crate::responses_metadata::TOOL_NAMESPACES_INFO_KEY;
use crate::responses_metadata::TURN_TRIGGER_KEY;
use crate::responses_metadata::WINDOW_ID_KEY;
use crate::responses_metadata::WINDOW_NUMBER_KEY;
use crate::responses_metadata::validate_extra_metadata;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::update_turn_settings_for_test;
use codex_protocol::AgentPath;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::PathBufExt;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command;

fn test_mcp_turn_metadata_context() -> ExecutionMetadata<'static> {
    ExecutionMetadata {
        model: "gpt-5.4",
        reasoning_effort: Some(ReasoningEffortConfig::High),
    }
}

fn test_turn_responses_metadata(
    state: &TurnMetadataState,
    window_id: &str,
) -> CodexResponsesMetadata {
    state.to_responses_metadata(
        "installation-a".to_string(),
        window_id.to_string(),
        CodexResponsesRequestKind::Turn,
    )
}

fn test_turn_responses_metadata_json(state: &TurnMetadataState, window_id: &str) -> String {
    test_turn_responses_metadata(state, window_id)
        .turn_metadata_json()
        .expect("turn metadata json")
}

fn test_compaction_responses_metadata_json(
    state: &TurnMetadataState,
    window_id: &str,
    compaction: CompactionTurnMetadata,
) -> String {
    state
        .to_responses_metadata(
            "installation-a".to_string(),
            window_id.to_string(),
            CodexResponsesRequestKind::Compaction(compaction),
        )
        .turn_metadata_json()
        .expect("turn metadata json")
}

fn test_turn_metadata_header(state: &TurnMetadataState) -> String {
    state
        .responses_metadata_template()
        .turn_metadata_json()
        .expect("header")
}

async fn create_clean_git_repo(repo_name: &str) -> (TempDir, AbsolutePathBuf) {
    let temp_dir = TempDir::new().expect("temp dir");
    let repo_path = temp_dir.path().join(repo_name).abs();
    std::fs::create_dir_all(&repo_path).expect("create repo");

    Command::new("git")
        .args(["init"])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git init");
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git config user.name");
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git config user.email");
    std::fs::write(repo_path.join("README.md"), "hello").expect("write file");
    Command::new("git")
        .args(["add", "."])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git commit");

    (temp_dir, repo_path)
}

async fn wait_for_git_enrichment(state: &TurnMetadataState) -> Value {
    tokio::time::timeout(Duration::from_secs(2), state.wait_for_git_enrichment())
        .await
        .expect("git enrichment should complete");
    serde_json::from_str(&test_turn_metadata_header(state)).expect("json")
}

#[tokio::test]
async fn detached_memory_responses_metadata_starts_an_independent_root_turn() {
    let (_temp_dir, repo_path) = create_clean_git_repo("repo-東京").await;

    let thread_manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("test"),
        crate::config::test_config().await.model_provider,
    );

    let metadata = detached_memory_responses_metadata(
        &thread_manager,
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        &SessionSource::Unknown,
        &repo_path,
    )
    .await;
    let header = metadata.turn_metadata_json().expect("header");
    assert!(header.is_ascii());
    assert!(!header.contains("東京"));
    let parsed: Value = serde_json::from_str(&header).expect("valid json");
    assert_eq!(parsed["request_kind"].as_str(), Some("memory"));
    assert_eq!(
        parsed["thread_source"].as_str(),
        Some("memory_consolidation")
    );
    assert!(parsed.get("session_id").is_none());
    assert!(parsed.get("thread_id").is_none());
    assert!(parsed.get("forked_from_thread_id").is_none());
    let turn_id = parsed["turn_id"].as_str().expect("memory turn ID");
    uuid::Uuid::parse_str(turn_id).expect("memory turn ID is a UUID");
    assert_eq!(parsed[ROOT_TURN_ID_KEY], parsed["turn_id"]);
    let client_metadata = metadata.client_metadata(/*include_internal*/ true);
    assert_eq!(
        client_metadata.get("turn_id").map(String::as_str),
        Some(turn_id)
    );
    assert_eq!(
        client_metadata.get(ROOT_TURN_ID_KEY).map(String::as_str),
        Some(turn_id)
    );
    assert!(parsed.get(WINDOW_ID_KEY).is_none());

    let expected_repo_path = repo_path.to_string_lossy().into_owned();
    let actual_repo_path = parsed
        .get("workspaces")
        .and_then(Value::as_object)
        .and_then(|workspaces| workspaces.keys().next())
        .expect("workspace path");
    assert_eq!(actual_repo_path, &expected_repo_path);
    let workspace = parsed
        .get("workspaces")
        .and_then(Value::as_object)
        .and_then(|workspaces| workspaces.values().next())
        .cloned()
        .expect("workspace");
    assert_eq!(
        workspace.get("has_changes").and_then(Value::as_bool),
        Some(false)
    );
}

#[tokio::test]
async fn detached_memory_responses_metadata_omits_empty_workspace_metadata() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let thread_manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("test"),
        crate::config::test_config().await.model_provider,
    );

    let header = detached_memory_responses_metadata(
        &thread_manager,
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        &SessionSource::Unknown,
        &cwd,
    )
    .await
    .turn_metadata_json()
    .expect("detached memory should emit its request kind");
    let parsed: Value = serde_json::from_str(&header).expect("valid json");
    let turn_id = parsed["turn_id"].as_str().expect("memory turn ID");
    uuid::Uuid::parse_str(turn_id).expect("memory turn ID is a UUID");

    assert_eq!(
        parsed,
        serde_json::json!({
            "turn_id": turn_id,
            "root_turn_id": turn_id,
            "request_kind": "memory",
            "thread_source": "memory_consolidation",
            "turn_trigger": "memory_consolidation",
        })
    );
}

#[tokio::test(start_paused = true)]
async fn memory_workspaces_times_out_pending_git_discovery() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let workspaces = tokio::time::timeout(
        Duration::from_secs(2),
        memory_workspaces(&cwd, std::future::pending()),
    )
    .await
    .expect("memory metadata should stop waiting for Git discovery");

    assert!(workspaces.is_empty());
}

#[test]
fn turn_metadata_state_includes_root_fork_lineage() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let source_thread_id =
        ThreadId::from_string("11111111-1111-4111-8111-111111111111").expect("thread id");

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        Some(source_thread_id),
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );

    let header = test_turn_metadata_header(&state);
    let json: Value = serde_json::from_str(&header).expect("json");

    assert_eq!(
        json["forked_from_thread_id"].as_str(),
        Some("11111111-1111-4111-8111-111111111111")
    );
    assert!(json.get("parent_thread_id").is_none());
    assert!(json.get("subagent_kind").is_none());
}

#[test]
fn turn_metadata_state_includes_thread_spawn_subagent_parent_without_fork() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let parent_thread_id =
        ThreadId::from_string("22222222-2222-4222-8222-222222222222").expect("thread id");

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        Some(parent_thread_id),
        &SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: Some(AgentPath::root().join("worker").expect("agent path")),
            agent_nickname: None,
            agent_role: None,
        }),
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );

    let header = test_turn_metadata_header(&state);
    let json: Value = serde_json::from_str(&header).expect("json");

    assert!(json.get("forked_from_thread_id").is_none());
    assert_eq!(
        json["parent_thread_id"].as_str(),
        Some("22222222-2222-4222-8222-222222222222")
    );
    assert_eq!(json["subagent_kind"].as_str(), Some("thread_spawn"));
    assert_eq!(json["agent_name"].as_str(), Some("/root/worker"));
}

#[test]
fn turn_metadata_state_omits_fork_lineage_for_context_inheriting_subagent() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let parent_thread_id =
        ThreadId::from_string("33333333-3333-4333-8333-333333333333").expect("thread id");

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        Some(parent_thread_id),
        Some(parent_thread_id),
        &SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );

    let header = test_turn_metadata_header(&state);
    let json: Value = serde_json::from_str(&header).expect("json");

    assert!(json.get("forked_from_thread_id").is_none());
    assert!(json.get(FORKED_FROM_ORDINAL_EXCLUSIVE_KEY).is_none());
    assert_eq!(
        json["parent_thread_id"].as_str(),
        Some("33333333-3333-4333-8333-333333333333")
    );
    assert_eq!(json["subagent_kind"].as_str(), Some("thread_spawn"));
    // V1 subagents have no canonical agent path and are intentionally unsupported by
    // agent-name-addressed history and notes; their metadata falls back to the root agent.
    assert_eq!(json["agent_name"].as_str(), Some("/root"));

    let mcp_metadata = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("MCP request metadata");
    assert_eq!(
        mcp_metadata["forked_from_thread_id"].as_str(),
        Some("33333333-3333-4333-8333-333333333333")
    );
}

#[test]
fn turn_metadata_state_includes_known_parent_for_non_thread_spawn_subagents_without_fork() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let parent_thread_id =
        ThreadId::from_string("44444444-4444-4444-8444-444444444444").expect("thread id");
    let sources = [
        (SubAgentSource::Review, "review"),
        (SubAgentSource::Other("guardian".to_string()), "guardian"),
    ];

    for (subagent_source, subagent_kind) in sources {
        let state = TurnMetadataState::new(
            "session-a".to_string(),
            "thread-a".to_string(),
            /*forked_from_thread_id*/ None,
            Some(parent_thread_id),
            &SessionSource::SubAgent(subagent_source),
            /*thread_source*/ None,
            "turn-a".to_string(),
            cwd.clone(),
        );

        let header = test_turn_metadata_header(&state);
        let json: Value = serde_json::from_str(&header).expect("json");

        assert!(json.get("forked_from_thread_id").is_none());
        assert_eq!(
            json["parent_thread_id"].as_str(),
            Some("44444444-4444-4444-8444-444444444444")
        );
        assert_eq!(json["subagent_kind"].as_str(), Some(subagent_kind));
        assert_eq!(json["agent_name"].as_str(), Some("/root"));
    }
}

#[test]
fn turn_metadata_state_includes_turn_started_at_unix_ms_after_start() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );
    state.set_turn_started_at_unix_ms(/*turn_started_at_unix_ms*/ 1_700_000_000_123);

    let header = test_turn_metadata_header(&state);
    let json: Value = serde_json::from_str(&header).expect("json");

    assert_eq!(
        json["turn_started_at_unix_ms"].as_i64(),
        Some(1_700_000_000_123)
    );
}

#[test]
fn turn_metadata_state_includes_model_and_reasoning_effort_only_in_request_meta() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );

    let header = test_turn_metadata_header(&state);
    let header_json: Value = serde_json::from_str(&header).expect("json");
    assert_eq!(header_json["agent_name"].as_str(), Some("/root"));
    assert!(header_json.get("model").is_none());
    assert!(header_json.get("reasoning_effort").is_none());

    let meta = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("turn metadata should be present");
    assert!(meta.get("agent_name").is_none());
    assert!(meta.get("request_kind").is_none());
    assert_eq!(meta["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(meta["reasoning_effort"].as_str(), Some("high"));

    let meta_without_reasoning_effort = state
        .current_meta_value_for_mcp_request(ExecutionMetadata {
            model: "gpt-5.4",
            reasoning_effort: None,
        })
        .expect("turn metadata should be present");
    assert_eq!(
        meta_without_reasoning_effort["model"].as_str(),
        Some("gpt-5.4")
    );
    assert!(
        meta_without_reasoning_effort
            .get("reasoning_effort")
            .is_none()
    );
}

#[tokio::test]
async fn execution_metadata_applies_captured_settings_to_responses_metadata() {
    let (_session, mut turn) = make_session_and_context().await;
    update_turn_settings_for_test(&mut turn, |settings| {
        let model_info = Arc::make_mut(&mut settings.model_info);
        model_info.slug = "captured-step-model".to_string();
    });
    let step_context = StepContext::for_test(Arc::new(turn));

    let mut metadata = step_context.turn.turn_metadata_state.to_responses_metadata(
        "installation-a".to_string(),
        "window-a".to_string(),
        CodexResponsesRequestKind::Turn,
    );
    ExecutionMetadata::from_settings(&step_context.settings).apply_to(&mut metadata);
    let metadata = metadata.turn_metadata_value().expect("turn metadata");

    assert_eq!(metadata["model"], "captured-step-model");
}

#[test]
fn turn_metadata_state_marks_user_input_requested_during_turn_only_for_mcp_request_meta() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );

    let header = test_turn_metadata_header(&state);
    let header_json: Value = serde_json::from_str(&header).expect("json");
    assert!(
        header_json
            .get(USER_INPUT_REQUESTED_DURING_TURN_KEY)
            .is_none()
    );

    let meta = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("turn metadata should be present");
    assert!(meta.get(USER_INPUT_REQUESTED_DURING_TURN_KEY).is_none());

    state.mark_user_input_requested_during_turn();

    let header = test_turn_metadata_header(&state);
    let header_json: Value = serde_json::from_str(&header).expect("json");
    assert!(
        header_json
            .get(USER_INPUT_REQUESTED_DURING_TURN_KEY)
            .is_none()
    );

    let meta = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("turn metadata should be present");
    assert_eq!(
        meta.get(USER_INPUT_REQUESTED_DURING_TURN_KEY)
            .and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn turn_metadata_state_ignores_client_reserved_metadata_before_start() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );
    state.set_responsesapi_client_metadata(HashMap::from([
        (
            LEGACY_CODE_MODE_TOOL_NAMES_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        (
            TOOL_NAMESPACES_INFO_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        (
            "turn_started_at_unix_ms".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "forked_from_thread_id".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "parent_thread_id".to_string(),
            "client-supplied".to_string(),
        ),
        ("parent_turn_id".to_string(), "client-supplied".to_string()),
        (ROOT_TURN_ID_KEY.to_string(), "client-supplied".to_string()),
        ("subagent_kind".to_string(), "client-supplied".to_string()),
        (
            "history_initialization".to_string(),
            "client-supplied".to_string(),
        ),
    ]));

    let header = test_turn_metadata_header(&state);
    let json: Value = serde_json::from_str(&header).expect("json");

    assert!(json.get(LEGACY_CODE_MODE_TOOL_NAMES_KEY).is_none());
    assert!(json.get(TOOL_NAMESPACES_INFO_KEY).is_none());
    assert!(json.get("turn_started_at_unix_ms").is_none());
    assert!(json.get("forked_from_thread_id").is_none());
    assert!(json.get("parent_thread_id").is_none());
    assert!(json.get("parent_turn_id").is_none());
    assert!(json.get(ROOT_TURN_ID_KEY).is_none());
    assert!(json.get("subagent_kind").is_none());
    assert!(json.get("history_initialization").is_none());
}

#[test]
fn turn_metadata_state_merges_client_metadata_without_replacing_reserved_fields() {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let source_thread_id =
        ThreadId::from_string("44444444-4444-4444-8444-444444444444").expect("thread id");
    let parent_thread_id =
        ThreadId::from_string("55555555-5555-4555-8555-555555555555").expect("thread id");

    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        Some(source_thread_id),
        Some(parent_thread_id),
        &SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        Some(ThreadSource::Feature("automation".to_string())),
        "turn-a".to_string(),
        cwd,
    );
    state.set_responses_api_metadata(BTreeMap::from([
        ("codex_security_surface".to_string(), "sdk".to_string()),
        ("source".to_string(), " Configured_Source ".to_string()),
        (
            WINDOW_NUMBER_KEY.to_string(),
            "configured-value".to_string(),
        ),
        (
            FORKED_FROM_ORDINAL_EXCLUSIVE_KEY.to_string(),
            "configured-value".to_string(),
        ),
    ]));
    state.set_parent_turn_id("parent-turn-a".to_string());
    state.set_root_turn_id("root-turn-a".to_string());
    state.set_turn_trigger("goal".to_string());
    state.set_responsesapi_client_metadata(HashMap::from([
        (
            "codex_security_surface".to_string(),
            "client-supplied".to_string(),
        ),
        ("fiber_run_id".to_string(), "fiber-123".to_string()),
        ("origin".to_string(), "東京".to_string()),
        ("workspace_kind".to_string(), "projectless".to_string()),
        ("source".to_string(), "client-source".to_string()),
        ("model".to_string(), "client-supplied".to_string()),
        ("codex_version".to_string(), "client-supplied".to_string()),
        (
            "reasoning_effort".to_string(),
            "client-supplied".to_string(),
        ),
        ("session_id".to_string(), "client-supplied".to_string()),
        ("thread_id".to_string(), "client-supplied".to_string()),
        ("agent_name".to_string(), "client-supplied".to_string()),
        ("installation_id".to_string(), "client-supplied".to_string()),
        (
            "x-codex-installation-id".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "x-codex-parent-thread-id".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "x-openai-subagent".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "forked_from_thread_id".to_string(),
            "client-supplied".to_string(),
        ),
        (
            "parent_thread_id".to_string(),
            "client-supplied".to_string(),
        ),
        ("parent_turn_id".to_string(), "client-supplied".to_string()),
        (ROOT_TURN_ID_KEY.to_string(), "client-supplied".to_string()),
        ("subagent_kind".to_string(), "client-supplied".to_string()),
        (
            LEGACY_CODE_MODE_TOOL_NAMES_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        (
            TOOL_NAMESPACES_INFO_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        ("turn_id".to_string(), "client-supplied".to_string()),
        (WINDOW_ID_KEY.to_string(), "client-supplied".to_string()),
        (WINDOW_NUMBER_KEY.to_string(), "client-supplied".to_string()),
        (
            CONTEXT_WINDOW_ID_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        (
            FORKED_FROM_ORDINAL_EXCLUSIVE_KEY.to_string(),
            "client-supplied".to_string(),
        ),
        ("thread_source".to_string(), "client-supplied".to_string()),
        (TURN_TRIGGER_KEY.to_string(), "client-supplied".to_string()),
        ("request_kind".to_string(), "client-supplied".to_string()),
        (
            "turn_started_at_unix_ms".to_string(),
            "client-supplied".to_string(),
        ),
    ]));
    state.set_turn_started_at_unix_ms(/*turn_started_at_unix_ms*/ 1_700_000_000_123);
    let header = test_turn_metadata_header(&state);
    assert!(header.is_ascii());
    assert!(!header.contains("東京"));
    let json: Value = serde_json::from_str(&header).expect("json");

    assert_eq!(json["fiber_run_id"].as_str(), Some("fiber-123"));
    assert_eq!(json["origin"].as_str(), Some("東京"));
    assert_eq!(json["workspace_kind"].as_str(), Some("projectless"));
    assert_eq!(json["codex_security_surface"].as_str(), Some("sdk"));
    assert_eq!(json["model"].as_str(), Some("client-supplied"));
    assert_eq!(json["reasoning_effort"].as_str(), Some("client-supplied"));
    assert_eq!(json["session_id"].as_str(), Some("session-a"));
    assert_eq!(json["thread_id"].as_str(), Some("thread-a"));
    assert!(json.get(LEGACY_CODE_MODE_TOOL_NAMES_KEY).is_none());
    assert_eq!(json["agent_name"].as_str(), Some("/root"));
    assert!(json.get(TOOL_NAMESPACES_INFO_KEY).is_none());
    assert!(json.get(INSTALLATION_ID_KEY).is_none());
    assert!(json.get("x-codex-installation-id").is_none());
    assert!(json.get("x-codex-parent-thread-id").is_none());
    assert!(json.get("x-openai-subagent").is_none());
    assert!(json.get("forked_from_thread_id").is_none());
    assert!(json.get(FORKED_FROM_ORDINAL_EXCLUSIVE_KEY).is_none());
    assert_eq!(
        json["parent_thread_id"].as_str(),
        Some("55555555-5555-4555-8555-555555555555")
    );
    assert_eq!(json["parent_turn_id"].as_str(), Some("parent-turn-a"));
    assert_eq!(json[ROOT_TURN_ID_KEY].as_str(), Some("root-turn-a"));
    assert_eq!(json["subagent_kind"].as_str(), Some("thread_spawn"));
    assert_eq!(json["thread_source"].as_str(), Some("automation"));
    assert_eq!(json[TURN_TRIGGER_KEY].as_str(), Some("goal"));
    assert_eq!(json["turn_id"].as_str(), Some("turn-a"));
    assert!(json.get("request_kind").is_none());
    assert!(json.get(WINDOW_ID_KEY).is_none());
    assert!(json.get(WINDOW_NUMBER_KEY).is_none());
    assert!(json.get(CONTEXT_WINDOW_ID_KEY).is_none());
    assert_eq!(
        json["turn_started_at_unix_ms"].as_i64(),
        Some(1_700_000_000_123)
    );

    let model_request_header = test_turn_responses_metadata_json(&state, "thread-a:1");
    let model_request_json: Value =
        serde_json::from_str(&model_request_header).expect("model request json");
    assert_eq!(model_request_json["request_kind"].as_str(), Some("turn"));
    assert_eq!(
        model_request_json[ROOT_TURN_ID_KEY].as_str(),
        Some("root-turn-a")
    );
    assert_eq!(
        model_request_json["thread_source"].as_str(),
        Some("automation")
    );
    assert_eq!(
        model_request_json["codex_security_surface"].as_str(),
        Some("sdk")
    );
    assert_eq!(
        model_request_json[INSTALLATION_ID_KEY].as_str(),
        Some("installation-a")
    );
    assert_eq!(
        model_request_json[WINDOW_ID_KEY].as_str(),
        Some("thread-a:1")
    );

    let compatibility_headers =
        test_turn_responses_metadata(&state, "thread-a:1").compatibility_headers();
    let compatibility_metadata: Value = serde_json::from_str(
        compatibility_headers
            .get("x-codex-turn-metadata")
            .expect("compatibility turn metadata header")
            .to_str()
            .expect("valid compatibility header"),
    )
    .expect("compatibility metadata json");
    assert!(
        compatibility_metadata
            .get(LEGACY_CODE_MODE_TOOL_NAMES_KEY)
            .is_none()
    );
    assert!(
        compatibility_metadata
            .get(TOOL_NAMESPACES_INFO_KEY)
            .is_none()
    );

    let meta = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("turn metadata should be present");
    assert_eq!(meta["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(meta["codex_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(meta["reasoning_effort"].as_str(), Some("high"));
    assert!(meta.get(LEGACY_CODE_MODE_TOOL_NAMES_KEY).is_none());
    assert!(meta.get(TOOL_NAMESPACES_INFO_KEY).is_none());
    assert!(meta.get(PARENT_TURN_ID_KEY).is_none());
    assert!(meta.get(ROOT_TURN_ID_KEY).is_none());
    assert!(meta.get(WINDOW_ID_KEY).is_none());
    assert!(meta.get("codex_security_surface").is_none());
    assert_eq!(state.current_turn_trigger().as_deref(), Some("goal"));
    assert_eq!(model_request_json["source"], " Configured_Source ");
}

#[test_case::test_case(r#"{"review_target":"RB._~:-opaque"}"#, None; "target")]
#[test_case::test_case(r#"{"timestamp":1750000000}"#, Some(r#"{"timestamp":1750000000}"#); "legacy_timestamp")]
fn turn_metadata_state_overlays_compaction_only_on_compaction_requests(
    continuation: &str,
    compaction_override: Option<&str>,
) {
    let temp_dir = TempDir::new().expect("temp dir");
    let cwd = temp_dir.path().abs();
    let state = TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        cwd,
    );
    state.set_responses_api_metadata(BTreeMap::from([
        ("codex_security_surface".to_string(), "sdk".to_string()),
        (
            "misalignment_override".to_string(),
            r#"{"review_target":"RC"}"#.to_string(),
        ),
    ]));
    let ordinary_json: Value =
        serde_json::from_str(&test_turn_responses_metadata_json(&state, "thread-a:1"))
            .expect("json");
    assert!(ordinary_json.get("misalignment_override").is_none());
    state.set_responsesapi_client_metadata(HashMap::from([
        ("compaction".to_string(), "client-supplied".to_string()),
        (
            "misalignment_override".to_string(),
            continuation.to_string(),
        ),
    ]));

    let mcp_metadata = state
        .current_meta_value_for_mcp_request(test_mcp_turn_metadata_context())
        .expect("MCP metadata");
    assert_eq!(
        mcp_metadata["misalignment_override"].as_str(),
        compaction_override
    );

    let compact_header = test_compaction_responses_metadata_json(
        &state,
        "thread-a:2",
        CompactionTurnMetadata::new(
            CompactionTrigger::Auto,
            CompactionReason::ContextLimit,
            CompactionImplementation::ResponsesCompactionV2,
            CompactionPhase::MidTurn,
        ),
    );
    let compact_json: Value = serde_json::from_str(&compact_header).expect("json");
    assert_eq!(compact_json["request_kind"].as_str(), Some("compaction"));
    assert_eq!(compact_json["turn_id"].as_str(), Some("turn-a"));
    assert_eq!(compact_json[WINDOW_ID_KEY].as_str(), Some("thread-a:2"));
    assert_eq!(compact_json["codex_security_surface"].as_str(), Some("sdk"));
    assert_eq!(
        compact_json["misalignment_override"].as_str(),
        compaction_override
    );
    assert_eq!(
        compact_json["compaction"],
        serde_json::json!({
            "trigger": "auto",
            "reason": "context_limit",
            "implementation": "responses_compaction_v2",
            "phase": "mid_turn",
            "strategy": "memento",
        })
    );

    let regular_header = test_turn_responses_metadata_json(&state, "thread-a:3");
    let regular_json: Value = serde_json::from_str(&regular_header).expect("json");
    assert_eq!(regular_json["misalignment_override"], continuation);
    assert_eq!(regular_json["request_kind"].as_str(), Some("turn"));
    assert_eq!(regular_json[WINDOW_ID_KEY].as_str(), Some("thread-a:3"));
    assert_eq!(regular_json["codex_security_surface"].as_str(), Some("sdk"));
    assert!(regular_json.get("compaction").is_none());
}

#[test]
fn responses_api_metadata_rejects_reserved_keys() {
    for reserved_key in [
        "thread_source",
        TURN_TRIGGER_KEY,
        WINDOW_ID_KEY,
        CONTEXT_WINDOW_ID_KEY,
        MCP_ATTRIBUTION_CLIENT_METADATA_KEY,
    ] {
        assert_eq!(
            validate_extra_metadata(
                BTreeMap::from([(reserved_key.to_string(), "sdk".to_string())]).iter()
            ),
            Err("responses_api_metadata contains a reserved key")
        );
    }
}

#[test]
fn responses_api_metadata_accepts_previously_valid_reserved_keys() {
    for legacy_key in [WINDOW_NUMBER_KEY, FORKED_FROM_ORDINAL_EXCLUSIVE_KEY] {
        assert_eq!(
            validate_extra_metadata(
                BTreeMap::from([(legacy_key.to_string(), "legacy-value".to_string())]).iter()
            ),
            Ok(())
        );
    }
}

#[tokio::test]
async fn turn_metadata_state_preserves_subagent_parent_after_git_enrichment() {
    let (_temp_dir, repo_path) = create_clean_git_repo("repo").await;

    let parent_thread_id =
        ThreadId::from_string("66666666-6666-4666-8666-666666666666").expect("thread id");
    let state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        Some(parent_thread_id),
        Some(parent_thread_id),
        &SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: None,
        }),
        /*thread_source*/ None,
        "turn-a".to_string(),
        repo_path,
    ));

    state.spawn_git_enrichment_task(Arc::default());
    let json = wait_for_git_enrichment(&state).await;

    assert!(json.get("forked_from_thread_id").is_none());
    assert_eq!(
        json["parent_thread_id"].as_str(),
        Some("66666666-6666-4666-8666-666666666666")
    );
    assert_eq!(json["subagent_kind"].as_str(), Some("thread_spawn"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_metadata_state_coalesces_concurrent_git_enrichment() {
    let (_temp_dir, repo_path) = create_clean_git_repo("repo").await;
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&repo_path)
        .output()
        .await
        .expect("git rev-parse HEAD");
    let head = String::from_utf8(head.stdout)
        .expect("commit hash")
        .trim()
        .to_string();
    let state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        repo_path.clone(),
    ));
    let git_root_discovery = Arc::default();
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let tasks = (0..8)
        .map(|_| {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            let git_root_discovery = Arc::clone(&git_root_discovery);
            tokio::spawn(async move {
                barrier.wait().await;
                state.spawn_git_enrichment_task(git_root_discovery);
                state
                    .enrichment_task
                    .lock()
                    .expect("enrichment task lock")
                    .as_ref()
                    .expect("enrichment task")
                    .abort_handle()
                    .id()
            })
        })
        .collect::<Vec<_>>();
    let mut task_ids = Vec::new();
    for task in tasks {
        task_ids.push(task.await.expect("spawn task"));
    }
    assert!(task_ids.iter().all(|task_id| *task_id == task_ids[0]));

    let json = wait_for_git_enrichment(state.as_ref()).await;
    assert_eq!(
        json["workspaces"],
        serde_json::json!({
            repo_path.to_string_lossy().as_ref(): {
                "latest_git_commit_hash": head,
                "has_changes": false,
            }
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_turn_metadata_aborts_unused_git_enrichment() {
    let temp_dir = TempDir::new().expect("temp dir");
    let state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        temp_dir.path().abs(),
    ));
    let weak_state = Arc::downgrade(&state);
    state.spawn_git_enrichment_task(Arc::default());
    let task = state
        .enrichment_task
        .lock()
        .expect("enrichment task lock")
        .as_ref()
        .expect("enrichment task")
        .abort_handle();

    // Drop the history-only context before its task can run. A strong task-owned
    // reference would retain the metadata even after every caller has left.
    drop(state);
    assert!(weak_state.upgrade().is_none());

    tokio::time::timeout(Duration::from_secs(2), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping turn metadata should abort its enrichment task");
}

#[tokio::test]
async fn git_enrichment_discovers_the_repository_when_it_starts() {
    let (workspace, repo_path) = create_clean_git_repo("repo").await;
    let saved_git = workspace.path().join("saved-git");
    std::fs::rename(repo_path.join(".git"), &saved_git).expect("move Git metadata");
    let state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        repo_path.clone(),
    ));
    std::fs::rename(saved_git, repo_path.join(".git")).expect("restore Git metadata");

    state.spawn_git_enrichment_task(Arc::default());
    let metadata = wait_for_git_enrichment(&state).await;
    assert_eq!(
        metadata["workspaces"]
            .as_object()
            .expect("workspace metadata")
            .keys()
            .collect::<Vec<_>>(),
        vec![&repo_path.to_string_lossy().into_owned()],
    );
}

#[tokio::test]
async fn turn_metadata_state_git_enrichment_cancellation_is_retryable_and_errors_stay_empty() {
    let (_temp_dir, repo_path) = create_clean_git_repo("repo").await;
    let state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-a".to_string(),
        repo_path,
    ));
    let git_root_discovery = Arc::default();
    state.spawn_git_enrichment_task(Arc::clone(&git_root_discovery));
    state.cancel_git_enrichment_task();
    assert!(
        state
            .enrichment_task
            .lock()
            .expect("enrichment task lock")
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(2), state.wait_for_git_enrichment())
        .await
        .expect("cancelled git enrichment should unblock waiters");
    assert!(state.current_workspaces().is_empty());

    state.spawn_git_enrichment_task(git_root_discovery);
    let json = wait_for_git_enrichment(&state).await;
    assert_eq!(
        json["workspaces"].as_object().map(serde_json::Map::len),
        Some(1)
    );

    let invalid_repo = TempDir::new().expect("invalid repo");
    std::fs::create_dir(invalid_repo.path().join(".git")).expect("invalid git directory");
    std::fs::write(
        invalid_repo.path().join(".git/HEAD"),
        "ref: refs/heads/main\n",
    )
    .expect("invalid git HEAD");
    let invalid_state = Arc::new(TurnMetadataState::new(
        "session-a".to_string(),
        "thread-a".to_string(),
        /*forked_from_thread_id*/ None,
        /*parent_thread_id*/ None,
        &SessionSource::Exec,
        /*thread_source*/ None,
        "turn-b".to_string(),
        invalid_repo.path().abs(),
    ));
    invalid_state.spawn_git_enrichment_task(Arc::default());
    tokio::time::timeout(
        Duration::from_secs(2),
        invalid_state.wait_for_git_enrichment(),
    )
    .await
    .expect("failed git enrichment should complete");
    assert!(invalid_state.current_workspaces().is_empty());
}

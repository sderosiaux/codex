use super::account::McpToolAccountError;
use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::tests::update_selected_settings_for_test;
use crate::state::ActiveTurn;
use crate::turn_metadata::ExecutionMetadata;
use codex_features::Features;
use codex_protocol::ResponseItemId;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::Instrument;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_test::internal::MockWriter;

fn annotations(
    read_only: Option<bool>,
    destructive: Option<bool>,
    open_world: Option<bool>,
) -> ToolAnnotations {
    ToolAnnotations::from_raw(
        /*title*/ None,
        read_only,
        destructive,
        /*idempotent_hint*/ None,
        open_world,
    )
}

#[test_case::test_case(false, None, Ok(Some("default_link")); "catalog_account")]
#[test_case::test_case(true, None, Err(McpToolAccountError::InvalidSelector); "required_selector_omitted")]
#[test_case::test_case(true, Some(serde_json::json!(null)), Err(McpToolAccountError::InvalidSelector); "null_selector")]
#[test_case::test_case(true, Some(serde_json::json!(42)), Err(McpToolAccountError::InvalidSelector); "invalid_selector")]
#[test_case::test_case(true, Some(serde_json::json!(" ")), Err(McpToolAccountError::InvalidSelector); "empty_selector")]
#[test_case::test_case(true, Some(serde_json::json!(" selected_link ")), Ok(Some(" selected_link ")); "opaque_selector")]
#[test_case::test_case(false, Some(serde_json::json!("selected_link")), Ok(Some("default_link")); "unrelated_link_argument")]
fn mcp_tool_metadata_resolves_advertised_account_selector(
    requires_explicit_link_id: bool,
    selected_link: Option<JsonValue>,
    expected: Result<Option<&str>, McpToolAccountError>,
) {
    let tool_info = serde_json::from_value(serde_json::json!({
        "server_name": CODEX_APPS_MCP_SERVER_NAME,
        "tool_name": "events/create",
        "tool_namespace": "calendar",
        "connector_id": "calendar",
        "tool": {
            "name": "calendar/events/create",
            "inputSchema": {},
            "_meta": {
                "link_id": "default_link",
                "_codex_apps": {
                    "requires_explicit_link_id": requires_explicit_link_id,
                },
            },
        },
    }))
    .expect("tool info");
    let arguments = selected_link.map(|link_id| serde_json::json!({ "link_id": link_id }));
    assert_eq!(
        mcp_tool_metadata(&tool_info, /*plugin_id*/ None, arguments.as_ref())
            .map(|metadata| metadata.link_id),
        expected.map(|link_id| link_id.map(str::to_owned)),
    );
}

#[test_case::test_case(None, Ok(None); "no_tool_metadata_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({})), Ok(None); "no_apps_metadata_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": {} })), Ok(None); "missing_selector_flag_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": false } })), Ok(None); "false_selector_flag_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": null } })), Ok(None); "null_selector_flag_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": "true" } })), Ok(None); "string_true_does_not_require_selector")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": 1 } })), Ok(None); "numeric_one_does_not_require_selector")]
#[test_case::test_case(Some(serde_json::json!({ "_codex_apps": [] })), Ok(None); "malformed_apps_metadata_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "link_id": null })), Ok(None); "null_catalog_link_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "link_id": 42, "_codex_apps": { "requires_explicit_link_id": false } })), Ok(None); "invalid_optional_catalog_link_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "link_id": " ", "_codex_apps": { "requires_explicit_link_id": false } })), Ok(None); "empty_optional_catalog_link_uses_legacy_fallback")]
#[test_case::test_case(Some(serde_json::json!({ "link_id": "default_link" })), Ok(Some("default_link")); "legacy_catalog_link_needs_no_selector_metadata")]
fn mcp_tool_metadata_preserves_legacy_account_fallback(
    meta: Option<JsonValue>,
    expected: Result<Option<&str>, McpToolAccountError>,
) {
    let mut tool = serde_json::json!({ "name": "calendar/events/create", "inputSchema": {} });
    if let Some(meta) = meta {
        tool["_meta"] = meta;
    }
    let tool_info = serde_json::from_value(serde_json::json!({
        "server_name": CODEX_APPS_MCP_SERVER_NAME,
        "tool_name": "events/create",
        "tool_namespace": "calendar",
        "connector_id": "calendar",
        "tool": tool,
    }))
    .expect("tool info");

    assert_eq!(
        mcp_tool_metadata(&tool_info, /*plugin_id*/ None, /*arguments*/ None)
            .map(|metadata| metadata.link_id),
        expected.map(|link_id| link_id.map(str::to_owned)),
    );
}

#[test_case::test_case(serde_json::json!({}); "no_account_metadata")]
#[test_case::test_case(serde_json::json!({ "_codex_apps": { "requires_explicit_link_id": true } }); "unrelated_apps_metadata")]
fn non_apps_tool_does_not_require_account_metadata(meta: JsonValue) {
    let tool_info = serde_json::from_value(serde_json::json!({
        "server_name": "custom_server",
        "tool_name": "events/create",
        "tool_namespace": "calendar",
        "tool": { "name": "calendar/events/create", "inputSchema": {}, "_meta": meta },
    }))
    .expect("tool info");

    assert_eq!(
        mcp_tool_metadata(&tool_info, /*plugin_id*/ None, /*arguments*/ None)
            .map(|metadata| metadata.link_id),
        Ok(None),
    );
}

fn mcp_turn_metadata_context(turn_context: &TurnContext) -> ExecutionMetadata<'_> {
    ExecutionMetadata {
        model: turn_context.model_info().slug.as_str(),
        reasoning_effort: turn_context.effective_reasoning_effort(),
    }
}

fn tool_metadata(
    connector_id: Option<&str>,
    connector_name: Option<&str>,
    tool_title: Option<&str>,
) -> McpToolMetadata {
    McpToolMetadata {
        annotations: None,
        connector_id: connector_id.map(str::to_string),
        link_id: None,
        connector_name: connector_name.map(str::to_string),
        plugin_id: None,
        tool_title: tool_title.map(str::to_string),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: None,
        openai_file_input_optional_fields: None,
    }
}

fn expected_mcp_turn_metadata(turn_context: &TurnContext) -> serde_json::Value {
    turn_context
        .turn_metadata_state
        .current_meta_value_for_mcp_request(mcp_turn_metadata_context(turn_context))
        .expect("turn metadata")
}

#[test]
fn mcp_app_resource_uri_reads_known_tool_meta_keys() {
    let nested = serde_json::json!({
        "ui": {
            "resourceUri": "ui://widget/nested.html",
        },
    });
    assert_eq!(
        get_mcp_app_resource_uri(nested.as_object()),
        Some("ui://widget/nested.html".to_string())
    );

    let flat = serde_json::json!({
        "ui/resourceUri": "ui://widget/flat.html",
    });
    assert_eq!(
        get_mcp_app_resource_uri(flat.as_object()),
        Some("ui://widget/flat.html".to_string())
    );

    let output_template = serde_json::json!({
        "openai/outputTemplate": "ui://widget/output-template.html",
    });
    assert_eq!(
        get_mcp_app_resource_uri(output_template.as_object()),
        Some("ui://widget/output-template.html".to_string())
    );
}

#[test]
fn openai_file_params_are_only_honored_for_codex_apps() {
    let params = HashMap::from([("file".to_string(), Vec::new())]);

    assert_eq!(
        openai_file_input_optional_fields_for_server(CODEX_APPS_MCP_SERVER_NAME, &params),
        Some(params.clone())
    );
    assert_eq!(
        openai_file_input_optional_fields_for_server("minimaltest", &params),
        None
    );
}

#[tokio::test]
async fn mcp_tool_call_span_records_expected_fields() {
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (session, turn_context) = make_session_and_context().await;

    async {}
        .instrument(mcp_tool_call_span(
            &session,
            &turn_context,
            McpToolCallSpanFields {
                server_name: "rmcp",
                tool_name: "echo",
                call_id: "call-123",
                server_origin: Some("https://example.com:8443/mcp"),
                connector_id: Some("calendar"),
                connector_name: Some("Calendar"),
            },
        ))
        .await;

    let logs = String::from_utf8(buffer.lock().expect("buffer lock").clone()).expect("utf8 logs");
    assert!(
        logs.contains("mcp.tools.call{otel.kind=\"client\"")
            && logs.contains("rpc.system=\"jsonrpc\"")
            && logs.contains("rpc.method=\"tools/call\"")
            && logs.contains("mcp.server.name=\"rmcp\"")
            && logs.contains("mcp.server.origin=\"https://example.com:8443/mcp\"")
            && logs.contains("mcp.transport=\"streamable_http\"")
            && logs.contains("mcp.connector.id=\"calendar\"")
            && logs.contains("mcp.connector.name=\"Calendar\"")
            && logs.contains("tool.name=\"echo\"")
            && logs.contains("tool.call_id=\"call-123\"")
            && logs.contains("server.address=\"example.com\"")
            && logs.contains("server.port=8443")
            && logs.contains("conversation.id=")
            && logs.contains("session.id=")
            && logs.contains("turn.id="),
        "missing MCP tool span fields\nlogs:\n{logs}"
    );
}

#[tokio::test]
async fn mcp_tool_call_span_records_error_type_and_error_code() {
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (session, turn_context) = make_session_and_context().await;
    let result = Ok(CallToolResult {
        content: Vec::new(),
        structured_content: Some(serde_json::json!({"error_code": "RATE_LIMITED"})),
        is_error: Some(true),
        meta: None,
    });
    let span = mcp_tool_call_span(
        &session,
        &turn_context,
        McpToolCallSpanFields {
            server_name: CODEX_APPS_MCP_SERVER_NAME,
            tool_name: "calendar_search",
            call_id: "call-123",
            server_origin: Some("https://chatgpt.com/api/codex/ps/mcp"),
            connector_id: Some("calendar"),
            connector_name: Some("Calendar"),
        },
    );

    async {
        record_mcp_result_span_telemetry(&Span::current(), &result);
    }
    .instrument(span)
    .await;

    let logs = String::from_utf8(buffer.lock().expect("buffer lock").clone()).expect("utf8 logs");
    assert!(
        logs.contains("error.type=\"tool_result\"")
            && logs.contains("codex.mcp.error.code=\"RATE_LIMITED\""),
        "missing MCP tool error span fields\nlogs:\n{logs}"
    );
}

async fn mcp_result_telemetry_span_logs(meta: Option<serde_json::Value>) -> String {
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_level(true)
        .with_ansi(false)
        .with_max_level(Level::TRACE)
        .with_span_events(FmtSpan::FULL)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (session, turn_context) = make_session_and_context().await;
    let result = Ok(CallToolResult {
        content: Vec::new(),
        structured_content: None,
        is_error: None,
        meta,
    });

    {
        let span = mcp_tool_call_span(
            &session,
            &turn_context,
            McpToolCallSpanFields {
                server_name: "rmcp",
                tool_name: "echo",
                call_id: "call-123",
                server_origin: None,
                connector_id: None,
                connector_name: None,
            },
        );

        async {
            record_mcp_result_span_telemetry(&Span::current(), &result);
        }
        .instrument(span)
        .await;
    }

    String::from_utf8(buffer.lock().expect("buffer lock").clone()).expect("utf8 logs")
}

#[tokio::test]
async fn mcp_result_telemetry_records_allowlisted_span_fields() {
    let logs = mcp_result_telemetry_span_logs(Some(serde_json::json!({
        "codex/telemetry": {
            "span": {
                "target_id": "com.apple.reminders",
                "did_trigger_server_user_flow": false,
                "not_promoted_sentinel_key": "not_promoted_sentinel_value",
            },
        },
    })))
    .await;

    assert!(
        logs.contains("codex.mcp.target.id=\"com.apple.reminders\"")
            && logs.contains("codex.mcp.server_user_flow.triggered=false"),
        "missing MCP result telemetry span fields\nlogs:\n{logs}"
    );
    assert!(
        !logs.contains("not_promoted_sentinel_key")
            && !logs.contains("not_promoted_sentinel_value"),
        "unknown MCP result telemetry keys should be ignored\nlogs:\n{logs}"
    );
}

#[tokio::test]
async fn mcp_result_telemetry_ignores_invalid_and_missing_values() {
    let invalid_logs = mcp_result_telemetry_span_logs(Some(serde_json::json!({
        "codex/telemetry": {
            "span": {
                "target_id": 123,
                "did_trigger_server_user_flow": "false",
            },
        },
    })))
    .await;
    assert!(
        !invalid_logs.contains("codex.mcp.target.id=")
            && !invalid_logs.contains("codex.mcp.server_user_flow.triggered="),
        "invalid MCP result telemetry values should be ignored\nlogs:\n{invalid_logs}"
    );

    let missing_logs = mcp_result_telemetry_span_logs(Some(serde_json::json!({
        "codex/telemetry": {},
    })))
    .await;
    assert!(
        !missing_logs.contains("codex.mcp.target.id=")
            && !missing_logs.contains("codex.mcp.server_user_flow.triggered="),
        "missing MCP result telemetry span object should be ignored\nlogs:\n{missing_logs}"
    );

    let no_meta_logs = mcp_result_telemetry_span_logs(/*meta*/ None).await;
    assert!(
        !no_meta_logs.contains("codex.mcp.target.id=")
            && !no_meta_logs.contains("codex.mcp.server_user_flow.triggered="),
        "missing MCP result metadata should be ignored\nlogs:\n{no_meta_logs}"
    );
}

#[tokio::test]
async fn mcp_result_telemetry_truncates_long_target_id() {
    let truncated = "x".repeat(MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS);
    let target_id = format!("{truncated}tail");
    let logs = mcp_result_telemetry_span_logs(Some(serde_json::json!({
        "codex/telemetry": {
            "span": {
                "target_id": target_id,
            },
        },
    })))
    .await;

    assert!(
        logs.contains(&format!("codex.mcp.target.id=\"{truncated}\"")) && !logs.contains("tail"),
        "long MCP result telemetry target_id should be truncated\nlogs:\n{logs}"
    );
}

#[test]
fn truncates_strings_on_char_boundaries() {
    let prefix = "á".repeat(MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS);
    let value = format!("{prefix}tail");
    let truncated = truncate_str_to_char_boundary(&value, MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS);

    assert_eq!(truncated, prefix);
    assert_eq!(
        truncate_str_to_char_boundary("short", MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS),
        "short"
    );
}

#[test]
fn sanitize_mcp_tool_result_for_model_rewrites_unsupported_media_content() {
    let result = Ok(CallToolResult {
        content: vec![
            serde_json::json!({
                "type": "image",
                "data": "Zm9v",
                "mimeType": "image/png",
            }),
            serde_json::json!({
                "type": "audio",
                "data": "YmFy",
                "mimeType": "audio/wav",
            }),
            serde_json::json!({
                "type": "text",
                "text": "hello",
            }),
        ],
        structured_content: None,
        is_error: Some(false),
        meta: None,
    });

    let got = sanitize_mcp_tool_result_for_model(&[InputModality::Text], result)
        .expect("sanitized result");

    assert_eq!(
        got,
        CallToolResult {
            content: vec![
                serde_json::json!({
                    "type": "text",
                    "text": "<image content omitted because you do not support image input>",
                }),
                serde_json::json!({
                    "type": "text",
                    "text": "<audio content omitted because you do not support audio input>",
                }),
                serde_json::json!({
                    "type": "text",
                    "text": "hello",
                }),
            ],
            structured_content: None,
            is_error: Some(false),
            meta: None,
        }
    );
}

#[test]
fn sanitize_mcp_tool_result_for_model_preserves_supported_media() {
    let original = CallToolResult {
        content: vec![
            serde_json::json!({
                "type": "image",
                "data": "Zm9v",
                "mimeType": "image/png",
            }),
            serde_json::json!({
                "type": "audio",
                "data": "YmFy",
                "mimeType": "audio/wav",
            }),
        ],
        structured_content: Some(serde_json::json!({"x": 1})),
        is_error: Some(false),
        meta: Some(serde_json::json!({"k": "v"})),
    };

    let got = sanitize_mcp_tool_result_for_model(
        &[
            InputModality::Text,
            InputModality::Image,
            InputModality::Audio,
        ],
        Ok(original.clone()),
    )
    .expect("unsanitized result");

    assert_eq!(got, original);
}

#[test]
fn truncate_mcp_tool_result_for_event_bounds_large_error() {
    let got = truncate_mcp_tool_result_for_event(&Err("error-message-".repeat(200_000)))
        .expect_err("large error should remain an error");

    // `truncate_text` includes its own marker, so allow a small amount of
    // overhead beyond the requested byte budget.
    assert!(got.len() < MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES + 1024);
    assert!(got.contains("truncated"));
}

#[tokio::test]
async fn mcp_tool_call_request_meta_includes_turn_metadata_for_custom_server() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);

    let meta = build_mcp_tool_call_request_meta(
        &step_context,
        "custom_server",
        "call-custom",
        /*metadata*/ None,
        /*is_host_owned_apps*/ false,
    )
    .expect("custom servers should receive turn metadata");
    let turn_metadata = meta
        .get(crate::X_CODEX_TURN_METADATA_HEADER)
        .expect("turn metadata should be present");

    assert_eq!(
        turn_metadata
            .get("model")
            .and_then(serde_json::Value::as_str),
        Some(step_context.settings.model_info.slug.as_str())
    );
    assert_eq!(
        turn_metadata
            .get("reasoning_effort")
            .and_then(serde_json::Value::as_str),
        step_context
            .settings
            .effective_reasoning_effort()
            .map(|effort| effort.to_string())
            .as_deref()
    );

    assert_eq!(
        meta,
        serde_json::json!({
            "callId": "call-custom",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
        })
    );
}

#[test_case::test_case(Some(ReasoningEffortConfig::Low), "low"; "configured effort")]
#[test_case::test_case(None, "high"; "pinned model default")]
#[tokio::test]
async fn mcp_tool_call_request_meta_uses_the_issuing_step(
    effort: Option<ReasoningEffortConfig>,
    expected_effort: &str,
) {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_a = StepContext::for_test(Arc::clone(&turn_context));
    let original_meta = build_mcp_tool_call_request_meta(
        &step_a,
        "node_repl",
        "call-a",
        /*metadata*/ None,
        /*is_host_owned_apps*/ false,
    );
    let mut step_b = StepContext::for_test(Arc::clone(&turn_context));
    let settings = Arc::make_mut(&mut Arc::get_mut(&mut step_b).expect("unique step").settings);
    update_selected_settings_for_test(settings, |selected| {
        selected.collaboration_mode = selected.collaboration_mode.with_updates(
            Some("model-b".to_string()),
            Some(effort),
            /*developer_instructions*/ None,
        );
    });
    let model = Arc::make_mut(&mut settings.model_info);
    model.slug = "model-b".to_string();
    model.default_reasoning_level = Some(ReasoningEffortConfig::High);

    let mut expected = expected_mcp_turn_metadata(&turn_context);
    expected["model"] = serde_json::json!("model-b");
    expected["reasoning_effort"] = serde_json::json!(expected_effort);
    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_b,
            "node_repl",
            "call-b",
            /*metadata*/ None,
            /*is_host_owned_apps*/ false
        ),
        Some(serde_json::json!({
            "callId": "call-b",
            crate::X_CODEX_TURN_METADATA_HEADER: expected,
        })),
    );
    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_a,
            "node_repl",
            "call-a",
            /*metadata*/ None,
            /*is_host_owned_apps*/ false
        ),
        original_meta,
    );
}

#[tokio::test]
async fn mcp_tool_call_request_meta_includes_turn_started_at_unix_ms() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    turn_context
        .turn_metadata_state
        .set_turn_started_at_unix_ms(/*turn_started_at_unix_ms*/ 1_700_000_000_123);

    let meta = build_mcp_tool_call_request_meta(
        &step_context,
        "custom_server",
        "call-custom",
        /*metadata*/ None,
        /*is_host_owned_apps*/ false,
    )
    .expect("custom servers should receive turn metadata");
    let turn_metadata = meta
        .get(crate::X_CODEX_TURN_METADATA_HEADER)
        .expect("turn metadata should be present");

    assert_eq!(
        turn_metadata
            .get("turn_started_at_unix_ms")
            .and_then(serde_json::Value::as_i64),
        Some(1_700_000_000_123)
    );
}

#[tokio::test]
async fn plugin_mcp_tool_call_request_meta_includes_plugin_id() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);
    let mut metadata = tool_metadata(
        /*connector_id*/ None, /*connector_name*/ None, /*tool_title*/ None,
    );
    metadata.plugin_id = Some("sample@test".to_string());

    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_context,
            "sample",
            "call-plugin",
            Some(&metadata),
            /*is_host_owned_apps*/ false
        ),
        Some(serde_json::json!({
            "callId": "call-plugin",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_PLUGIN_ID_META_KEY: "sample@test",
        }))
    );
}

#[test]
fn mcp_tool_call_item_metadata_only_trusts_codex_apps_identity() {
    let mut metadata = tool_metadata(
        Some("asdk_app_0123456789abcdef0123456789abcdef"),
        Some("Calendar"),
        Some("Create a calendar event"),
    );
    metadata.link_id = Some("link_fedcba9876543210fedcba9876543210".to_string());
    metadata.annotations = Some(annotations(
        Some(false),
        /*destructive*/ None,
        /*open_world*/ None,
    ));
    metadata.codex_apps_meta = Some(
        serde_json::json!({
            "resource_uri": "/asdk_app_0123456789abcdef0123456789abcdef/link_fedcba9876543210fedcba9876543210/create_event",
        })
        .as_object()
        .cloned()
        .expect("_codex_apps metadata should be an object"),
    );

    assert_eq!(
        McpToolCallItemMetadata::from_tool_metadata(CODEX_APPS_MCP_SERVER_NAME, Some(&metadata),),
        McpToolCallItemMetadata {
            connector_id: Some("asdk_app_0123456789abcdef0123456789abcdef".to_string()),
            link_id: Some("link_fedcba9876543210fedcba9876543210".to_string()),
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: Some("Calendar".to_string()),
            action_name: Some("create_event".to_string()),
            plugin_id: None,
            read_only_hint: Some(false),
        }
    );
    assert_eq!(
        McpToolCallItemMetadata::from_tool_metadata("custom_server", Some(&metadata)),
        McpToolCallItemMetadata {
            connector_id: None,
            link_id: None,
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: None,
            action_name: None,
            plugin_id: None,
            read_only_hint: Some(false),
        }
    );
}

#[tokio::test]
async fn mcp_tool_call_item_includes_app_identity() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;

    notify_mcp_tool_call_started(
        &session,
        &turn_context,
        "call-plugin",
        McpInvocation {
            server: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            tool: "echo".to_string(),
            arguments: None,
        },
        McpToolCallItemMetadata {
            connector_id: Some("asdk_app_0123456789abcdef0123456789abcdef".to_string()),
            link_id: Some("link_fedcba9876543210fedcba9876543210".to_string()),
            mcp_app_resource_uri: None,
            mcp_app_ui: None,
            app_name: Some("Calendar".to_string()),
            action_name: Some("create_event".to_string()),
            plugin_id: Some("sample@test".to_string()),
            read_only_hint: Some(false),
        },
    )
    .await;

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx_event.recv())
        .await
        .expect("tool call item timed out")
        .expect("tool call item event");
    let EventMsg::ItemStarted(item_started) = event.msg else {
        panic!("expected ItemStarted event");
    };
    let TurnItem::McpToolCall(item) = item_started.item else {
        panic!("expected MCP tool call item");
    };

    assert_eq!(
        item.connector_id.as_deref(),
        Some("asdk_app_0123456789abcdef0123456789abcdef")
    );
    assert_eq!(
        item.link_id.as_deref(),
        Some("link_fedcba9876543210fedcba9876543210")
    );
    assert_eq!(item.plugin_id.as_deref(), Some("sample@test"));
    assert_eq!(item.app_name.as_deref(), Some("Calendar"));
    assert_eq!(item.action_name.as_deref(), Some("create_event"));
    assert_eq!(item.read_only_hint, Some(false));
}

#[test_case::test_case(false; "user configured Apps name")]
#[test_case::test_case(true; "host owned Apps")]
#[tokio::test]
async fn codex_apps_tool_call_request_meta_includes_turn_metadata_and_codex_apps_meta(
    is_host_owned_apps: bool,
) {
    let (_, turn_context) = make_session_and_context().await;
    turn_context
        .turn_metadata_state
        .set_root_turn_id("causal-root".to_string());
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);
    let metadata = McpToolMetadata {
        annotations: None,
        connector_id: Some("calendar".to_string()),
        link_id: None,
        connector_name: Some("Calendar".to_string()),
        plugin_id: None,
        tool_title: Some("Create Event".to_string()),
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        codex_apps_meta: Some(
            serde_json::json!({
                "resource_uri": "connector://calendar/tools/calendar_create_event",
                "contains_mcp_source": true,
                "connector_id": "calendar",
                "root_turn_id": "untrusted-tool-metadata",
            })
            .as_object()
            .cloned()
            .expect("_codex_apps metadata should be an object"),
        ),
        openai_file_input_optional_fields: None,
    };

    let mut expected_apps_meta = serde_json::json!({
        "call_id": "call_abc123xyz789",
        "resource_uri": "connector://calendar/tools/calendar_create_event",
        "contains_mcp_source": true,
        "connector_id": "calendar",
    });
    if is_host_owned_apps {
        expected_apps_meta["root_turn_id"] = serde_json::json!("causal-root");
    }

    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_context,
            CODEX_APPS_MCP_SERVER_NAME,
            "call_abc123xyz789",
            Some(&metadata),
            is_host_owned_apps
        ),
        Some(serde_json::json!({
            "callId": "call_abc123xyz789",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_CODEX_APPS_META_KEY: expected_apps_meta,
        }))
    );
}

#[tokio::test]
async fn codex_apps_tool_call_request_meta_includes_call_id_without_existing_codex_apps_meta() {
    let (_, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let expected_turn_metadata = expected_mcp_turn_metadata(&turn_context);

    assert_eq!(
        build_mcp_tool_call_request_meta(
            &step_context,
            CODEX_APPS_MCP_SERVER_NAME,
            "call_abc123xyz789",
            /*metadata*/ None,
            /*is_host_owned_apps*/ false
        ),
        Some(serde_json::json!({
            "callId": "call_abc123xyz789",
            crate::X_CODEX_TURN_METADATA_HEADER: expected_turn_metadata,
            MCP_TOOL_CODEX_APPS_META_KEY: {
                "call_id": "call_abc123xyz789",
            },
        }))
    );
}

fn codex_apps_auth_failure_result() -> CallToolResult {
    CallToolResult {
        content: vec![serde_json::json!({
            "type": "text",
            "text": "Connector reauthentication required",
        })],
        structured_content: None,
        is_error: Some(true),
        meta: Some(serde_json::json!({
            MCP_TOOL_CODEX_APPS_META_KEY: {
                "connector_auth_failure": {
                    "is_auth_failure": true,
                    "auth_reason": "reauthentication_required",
                    "connector_id": "connector_calendar",
                    "connector_name": "Untrusted Calendar",
                    "link_id": "link_123",
                    "error_code": "UNAUTHORIZED",
                    "error_http_status_code": 401,
                    "error_action": "TRIGGER_REAUTHENTICATION",
                },
            },
        })),
    }
}

fn codex_apps_auth_failure_metadata() -> McpToolMetadata {
    tool_metadata(
        Some("connector_calendar"),
        Some("Google Calendar"),
        Some("Create Event"),
    )
}

#[tokio::test]
async fn codex_apps_auth_elicitation_feature_disabled_returns_original_result() {
    let (session, mut turn_context, rx_event) = make_session_and_context_with_rx().await;
    let mut features = Features::with_defaults();
    features.disable(Feature::AuthElicitation);
    let mutable_turn_context = Arc::get_mut(&mut turn_context).expect("single turn context ref");
    Arc::make_mut(&mut mutable_turn_context.config).features = features;
    let result = codex_apps_auth_failure_result();
    let metadata = codex_apps_auth_failure_metadata();

    let returned = maybe_request_codex_apps_auth_elicitation(
        &session,
        &turn_context,
        "call_123",
        CODEX_APPS_MCP_SERVER_NAME,
        Some(&metadata),
        result.clone(),
    )
    .await;

    assert_eq!(returned, result);
    assert!(rx_event.try_recv().is_err());
}

#[test_case::test_case(SessionSource::Exec; "root")]
#[test_case::test_case(SessionSource::SubAgent(SubAgentSource::Review); "subagent")]
#[tokio::test]
async fn codex_apps_auth_elicitation_enabled_by_default_requests_elicitation(
    source: SessionSource,
) {
    let (session, mut turn_context, rx_event) = make_session_and_context_with_rx().await;
    Arc::get_mut(&mut turn_context)
        .expect("single turn context ref")
        .session_source = source;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let result = codex_apps_auth_failure_result();
    let metadata = codex_apps_auth_failure_metadata();

    let request_task = tokio::spawn({
        let session = Arc::clone(&session);
        let turn_context = Arc::clone(&turn_context);
        async move {
            maybe_request_codex_apps_auth_elicitation(
                &session,
                &turn_context,
                "call_123",
                CODEX_APPS_MCP_SERVER_NAME,
                Some(&metadata),
                result,
            )
            .await
        }
    });

    let request = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx_event.recv())
            .await
            .expect("elicitation event timed out")
            .expect("expected elicitation event");
        if let EventMsg::ElicitationRequest(request) = event.msg {
            break request;
        }
    };
    assert_eq!(request.server_name, CODEX_APPS_MCP_SERVER_NAME);
    assert_eq!(
        request.id,
        codex_protocol::mcp::RequestId::String("codex_apps_auth_call_123".to_string())
    );
    assert!(matches!(
        request.request,
        codex_protocol::elicitation::ElicitationRequest::Url { .. }
    ));

    session
        .resolve_elicitation(
            CODEX_APPS_MCP_SERVER_NAME.to_string(),
            rmcp::model::RequestId::String("codex_apps_auth_call_123".into()),
            codex_rmcp_client::ElicitationResponse {
                action: ElicitationAction::Accept,
                content: None,
                meta: None,
            },
        )
        .await
        .expect("elicitation should resolve");
    let returned = tokio::time::timeout(std::time::Duration::from_secs(1), request_task)
        .await
        .expect("auth elicitation task timed out")
        .expect("auth elicitation task failed");
    assert_eq!(
        returned.content,
        vec![serde_json::json!({
            "type": "text",
            "text": "Authentication for Google Calendar was requested and accepted. Retry this tool call now.",
        })]
    );
}

#[test]
fn mcp_tool_call_ids_are_added_to_request_meta() {
    let origin = crate::tools::context::ToolCallOrigin {
        item_id: Some(ResponseItemId::from_server("fc-live".to_string())),
        window_id: "thread-live:2".to_string(),
    };

    assert_eq!(
        with_mcp_tool_call_ids_meta(
            Some(serde_json::json!({
                "source": "test-client",
                "threadId": "stale-thread",
                "sessionId": "stale-session",
                "windowId": "stale-window",
                "itemId": "stale-item",
            })),
            "thread-live",
            "session-live",
            &origin,
        ),
        Some(serde_json::json!({
            "source": "test-client",
            "threadId": "thread-live",
            "sessionId": "session-live",
            "itemId": "fc-live",
            "windowId": "thread-live:2",
        }))
    );

    let origin_without_item = crate::tools::context::ToolCallOrigin {
        item_id: None,
        window_id: "thread-live:3".to_string(),
    };
    assert_eq!(
        with_mcp_tool_call_ids_meta(
            /*meta*/ None,
            "thread-live",
            "session-live",
            &origin_without_item,
        ),
        Some(serde_json::json!({
            "threadId": "thread-live",
            "sessionId": "session-live",
            "windowId": "thread-live:3",
        }))
    );

    assert_eq!(
        with_mcp_tool_call_ids_meta(
            Some(serde_json::json!("invalid-meta")),
            "thread-live",
            "session-live",
            &origin_without_item,
        ),
        Some(serde_json::json!("invalid-meta"))
    );
}

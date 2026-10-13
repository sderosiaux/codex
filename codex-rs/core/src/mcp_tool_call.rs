use std::time::Duration;
use std::time::Instant;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallOrigin;
use crate::tools::lifecycle::process_mcp_tool_result;
use crate::turn_metadata::ExecutionMetadata;
use codex_extension_api::McpToolContext;
use codex_mcp::PreparedMcpCall;
use codex_mcp::ToolInfo;
use codex_protocol::items::McpAppDisplayMode;
use codex_protocol::items::McpAppUi;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::McpInvocation;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_mcp_tool_result;
use codex_utils_output_truncation::truncate_text;
use codex_utils_pty::DEFAULT_OUTPUT_BYTES_CAP;
use rmcp::model::ToolAnnotations;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tracing::Instrument;
use tracing::Span;
use tracing::error;
use tracing::field::Empty;
use url::Url;

mod telemetry;

use telemetry::emit_mcp_call_metrics;
use telemetry::mcp_call_metric_outcome;
use telemetry::record_mcp_call_outcome_span_telemetry;

const MCP_RESULT_TELEMETRY_META_KEY: &str = "codex/telemetry";
const MCP_RESULT_TELEMETRY_SPAN_KEY: &str = "span";
const MCP_RESULT_TELEMETRY_TARGET_ID_KEY: &str = "target_id";
const MCP_RESULT_TELEMETRY_DID_TRIGGER_SERVER_USER_FLOW_KEY: &str = "did_trigger_server_user_flow";
const MCP_RESULT_TELEMETRY_TARGET_ID_SPAN_ATTR: &str = "codex.mcp.target.id";
const MCP_RESULT_TELEMETRY_SERVER_USER_FLOW_SPAN_ATTR: &str =
    "codex.mcp.server_user_flow.triggered";
const MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS: usize = 256;
const MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES: usize = DEFAULT_OUTPUT_BYTES_CAP;

/// Handles the specified tool call and dispatches the appropriate MCP tool-call
/// item lifecycle events to the `Session`.
pub(crate) async fn handle_mcp_tool_call(
    sess: Arc<Session>,
    step_context: &Arc<StepContext>,
    call_id: String,
    originating_call: ToolCallOrigin,
    tool_info: &ToolInfo,
    prepared_call: Option<PreparedMcpCall>,
    arguments: String,
) -> HandledMcpToolCall {
    let turn_context = &step_context.turn;
    let server = tool_info.server_name.clone();
    let tool_name = tool_info.tool.name.to_string();
    // Parse the `arguments` as JSON. An empty string is OK, but invalid JSON
    // is not.
    let arguments_value = if arguments.trim().is_empty() {
        None
    } else {
        match serde_json::from_str::<serde_json::Value>(&arguments) {
            Ok(value) => Some(value),
            Err(e) => {
                error!("failed to parse tool call arguments: {e}");
                return HandledMcpToolCall {
                    result: CallToolResult::from_error_text(format!("err: {e}")),
                    tool_input: JsonValue::Object(serde_json::Map::new()),
                };
            }
        }
    };

    let invocation = McpInvocation {
        server: server.clone(),
        tool: tool_name.clone(),
        arguments: arguments_value.clone(),
    };

    let Some(prepared_call) = prepared_call else {
        let item_metadata = McpToolCallItemMetadata::from_tool_metadata(/*metadata*/ None);
        let result = notify_mcp_tool_call_skip(
            sess.as_ref(),
            turn_context.as_ref(),
            &call_id,
            invocation,
            item_metadata,
            format!("MCP tool `{server}/{tool_name}` is not available to the model"),
        )
        .await;
        return HandledMcpToolCall {
            result: CallToolResult::from_result(result),
            tool_input: arguments_value
                .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new())),
        };
    };
    let metadata = mcp_tool_metadata(prepared_call.tool_info());
    let item_metadata = McpToolCallItemMetadata::from_tool_metadata(Some(&metadata));
    notify_mcp_tool_call_started(
        sess.as_ref(),
        turn_context.as_ref(),
        &call_id,
        invocation.clone(),
        item_metadata.clone(),
    )
    .await;

    execute_mcp_tool_call(
        &sess,
        step_context.as_ref(),
        &call_id,
        &originating_call,
        invocation,
        prepared_call,
        item_metadata,
    )
    .await
}

pub(crate) struct HandledMcpToolCall {
    pub(crate) result: CallToolResult,
    pub(crate) tool_input: JsonValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpToolCallItemMetadata {
    mcp_app_resource_uri: Option<String>,
    mcp_app_ui: Option<McpAppUi>,
    read_only_hint: Option<bool>,
}

impl McpToolCallItemMetadata {
    fn from_tool_metadata(metadata: Option<&McpToolMetadata>) -> Self {
        Self {
            mcp_app_resource_uri: metadata
                .and_then(|metadata| metadata.mcp_app_resource_uri.clone()),
            mcp_app_ui: metadata.and_then(|metadata| metadata.mcp_app_ui.clone()),
            read_only_hint: metadata
                .and_then(|metadata| metadata.annotations.as_ref())
                .and_then(|annotations| annotations.read_only_hint),
        }
    }
}

async fn execute_mcp_tool_call(
    sess: &Arc<Session>,
    step_context: &StepContext,
    call_id: &str,
    originating_call: &ToolCallOrigin,
    invocation: McpInvocation,
    prepared_call: PreparedMcpCall,
    item_metadata: McpToolCallItemMetadata,
) -> HandledMcpToolCall {
    let turn_context = step_context.turn.as_ref();
    let server = invocation.server.clone();
    let tool_name = invocation.tool.clone();
    let arguments_value = invocation.arguments.clone();
    let server_origin = prepared_call.server_origin().map(str::to_string);

    let start = Instant::now();
    let tool_input = arguments_value
        .clone()
        .unwrap_or_else(|| JsonValue::Object(serde_json::Map::new()));
    let result = async {
        let result = async {
            let mut result = prepared_call
                .call_with_preparation(/*requested_timeout*/ None, || async {
                    let request_meta = build_mcp_tool_call_request_meta(step_context, call_id);
                    let request_meta = with_mcp_tool_call_ids_meta(
                        request_meta,
                        &sess.thread_id.to_string(),
                        &sess.session_id().to_string(),
                        originating_call,
                    );
                    Ok((None, request_meta))
                })
                .await
                .map_err(|error| format!("tool call error: {error:?}"))?;
            let mcp_tool = McpToolContext::from_prepared_call(
                &prepared_call,
                turn_context.config.mcp_servers.get(&server),
            );
            process_mcp_tool_result(
                sess,
                turn_context,
                call_id,
                &mcp_tool,
                &tool_input,
                &mut result,
            )
            .await;
            sanitize_mcp_tool_result_for_model(
                &step_context.settings.model_info.input_modalities,
                Ok(result),
            )
        }
        .await;
        record_mcp_result_span_telemetry(&Span::current(), &result);
        result
    }
    .instrument(mcp_tool_call_span(
        sess,
        turn_context,
        McpToolCallSpanFields {
            server_name: &server,
            tool_name: &tool_name,
            call_id,
            server_origin: server_origin.as_deref(),
        },
    ))
    .await;
    if let Err(error) = &result {
        tracing::warn!("MCP tool call error: {error:?}");
    }
    let duration = start.elapsed();
    // Once a call enters this boundary, conservatively attribute returned errors
    // too: they may contain peer data.
    sess.services.executed_tool_calls.record_mcp_source(
        codex_protocol::mcp::McpAttributionSource {
            server_name: prepared_call.server_name().to_string(),
            tool_name: prepared_call.tool_info().tool.name.to_string(),
            first_turn_id: turn_context.sub_id.clone(),
        },
    );
    notify_mcp_tool_call_completed(
        sess,
        turn_context,
        call_id,
        invocation,
        item_metadata,
        duration,
        truncate_mcp_tool_result_for_event(&result),
    )
    .await;

    let outcome = mcp_call_metric_outcome(&result);
    emit_mcp_call_metrics(
        &step_context.session_telemetry,
        &outcome,
        &server,
        &tool_name,
        Some(duration),
    );

    HandledMcpToolCall {
        result: CallToolResult::from_result(result),
        tool_input,
    }
}

fn mcp_tool_call_span(
    session: &Session,
    turn_context: &TurnContext,
    fields: McpToolCallSpanFields<'_>,
) -> Span {
    let transport = match fields.server_origin {
        Some("stdio") => "stdio",
        Some("in_process") => "in_process",
        Some(_) => "streamable_http",
        None => "",
    };
    let span = tracing::info_span!(
        "mcp.tools.call",
        otel.kind = "client",
        rpc.system = "jsonrpc",
        rpc.method = "tools/call",
        mcp.server.name = fields.server_name,
        mcp.server.origin = fields.server_origin.unwrap_or(""),
        mcp.transport = transport,
        tool.name = fields.tool_name,
        tool.call_id = fields.call_id,
        conversation.id = %session.thread_id,
        session.id = %session.thread_id,
        turn.id = turn_context.sub_id.as_str(),
        server.address = Empty,
        server.port = Empty,
        codex.mcp.target.id = Empty,
        codex.mcp.server_user_flow.triggered = Empty,
        error.type = Empty,
        codex.mcp.error.code = Empty,
    );
    record_server_fields(&span, fields.server_origin);
    span
}

struct McpToolCallSpanFields<'a> {
    server_name: &'a str,
    tool_name: &'a str,
    call_id: &'a str,
    server_origin: Option<&'a str>,
}

fn record_server_fields(span: &Span, url: Option<&str>) {
    let Some(url) = url else {
        return;
    };
    let Ok(parsed) = Url::parse(url) else {
        return;
    };
    if let Some(host) = parsed.host_str() {
        span.record("server.address", host);
    }
    if let Some(port) = parsed.port_or_known_default() {
        span.record("server.port", port as i64);
    }
}

fn record_mcp_result_span_telemetry(span: &Span, result: &Result<CallToolResult, String>) {
    record_mcp_call_outcome_span_telemetry(span, result);

    let Some(span_telemetry) = result
        .as_ref()
        .ok()
        .and_then(|result| result.meta.as_ref())
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(MCP_RESULT_TELEMETRY_META_KEY))
        .and_then(JsonValue::as_object)
        .and_then(|telemetry| telemetry.get(MCP_RESULT_TELEMETRY_SPAN_KEY))
        .and_then(JsonValue::as_object)
    else {
        return;
    };

    if let Some(target_id) = span_telemetry
        .get(MCP_RESULT_TELEMETRY_TARGET_ID_KEY)
        .and_then(JsonValue::as_str)
        .filter(|target_id| !target_id.is_empty())
    {
        span.record(
            MCP_RESULT_TELEMETRY_TARGET_ID_SPAN_ATTR,
            truncate_str_to_char_boundary(target_id, MCP_RESULT_TELEMETRY_TARGET_ID_MAX_CHARS),
        );
    }

    if let Some(did_trigger_server_user_flow) = span_telemetry
        .get(MCP_RESULT_TELEMETRY_DID_TRIGGER_SERVER_USER_FLOW_KEY)
        .and_then(JsonValue::as_bool)
    {
        span.record(
            MCP_RESULT_TELEMETRY_SERVER_USER_FLOW_SPAN_ATTR,
            did_trigger_server_user_flow,
        );
    }
}

fn truncate_str_to_char_boundary(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

fn sanitize_mcp_tool_result_for_model(
    input_modalities: &[InputModality],
    result: Result<CallToolResult, String>,
) -> Result<CallToolResult, String> {
    let supports_image_input = input_modalities.contains(&InputModality::Image);
    let supports_audio_input = input_modalities.contains(&InputModality::Audio);
    if supports_image_input && supports_audio_input {
        return result;
    }

    result.map(|call_tool_result| CallToolResult {
        content: call_tool_result
            .content
            .iter()
            .map(|block| {
                if let Some(content_type) = block.get("type").and_then(serde_json::Value::as_str) {
                    if content_type == "image" && !supports_image_input {
                        return serde_json::json!({
                            "type": "text",
                            "text": "<image content omitted because you do not support image input>",
                        });
                    }
                    if content_type == "audio" && !supports_audio_input {
                        return serde_json::json!({
                            "type": "text",
                            "text": "<audio content omitted because you do not support audio input>",
                        });
                    }
                }

                block.clone()
            })
            .collect::<Vec<_>>(),
        structured_content: call_tool_result.structured_content,
        is_error: call_tool_result.is_error,
        meta: call_tool_result.meta,
    })
}

fn truncate_mcp_tool_result_for_event(
    result: &Result<CallToolResult, String>,
) -> Result<CallToolResult, String> {
    match result {
        Ok(call_tool_result) => Ok(truncate_mcp_tool_result(
            call_tool_result,
            MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES,
        )
        .into_owned()),
        Err(message) => Err(truncate_text(
            message,
            TruncationPolicy::Bytes(MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES),
        )),
    }
}

async fn notify_mcp_tool_call_started(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
) {
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(JsonValue::Null),
        mcp_app_resource_uri: item_metadata.mcp_app_resource_uri,
        mcp_app_ui: item_metadata.mcp_app_ui,
        read_only_hint: item_metadata.read_only_hint,
        status: McpToolCallStatus::InProgress,
        result: None,
        error: None,
        duration: None,
    });
    sess.emit_turn_item_started(turn_context, &item).await;
}

async fn notify_mcp_tool_call_completed(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
    duration: Duration,
    result: Result<CallToolResult, String>,
) {
    let (status, result, error) = match result {
        Ok(result) if result.is_error.unwrap_or(false) => {
            (McpToolCallStatus::Failed, Some(result), None)
        }
        Ok(result) => (McpToolCallStatus::Completed, Some(result), None),
        Err(message) => (
            McpToolCallStatus::Failed,
            None,
            Some(McpToolCallError { message }),
        ),
    };
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(JsonValue::Null),
        mcp_app_resource_uri: item_metadata.mcp_app_resource_uri,
        mcp_app_ui: item_metadata.mcp_app_ui,
        read_only_hint: item_metadata.read_only_hint,
        status,
        result,
        error,
        duration: Some(duration),
    });
    sess.emit_turn_item_completed(turn_context, item).await;
}

#[derive(Clone)]
pub(crate) struct McpToolMetadata {
    annotations: Option<ToolAnnotations>,
    mcp_app_resource_uri: Option<String>,
    mcp_app_ui: Option<McpAppUi>,
}

const MCP_TOOL_OPENAI_OUTPUT_TEMPLATE_META_KEY: &str = "openai/outputTemplate";
const MCP_TOOL_UI_RESOURCE_URI_META_KEY: &str = "ui/resourceUri";
const MCP_TOOL_ITEM_ID_META_KEY: &str = "itemId";
const MCP_TOOL_SESSION_ID_META_KEY: &str = "sessionId";
const MCP_TOOL_WINDOW_ID_META_KEY: &str = "windowId";
const MCP_TOOL_THREAD_ID_META_KEY: &str = "threadId";

fn build_mcp_tool_call_request_meta(
    step_context: &StepContext,
    call_id: &str,
) -> Option<serde_json::Value> {
    let mut request_meta = serde_json::Map::new();
    request_meta.insert(
        "callId".to_string(),
        serde_json::Value::String(call_id.to_string()),
    );

    if let Some(turn_metadata) = step_context
        .turn
        .turn_metadata_state
        .current_meta_value_for_mcp_request(ExecutionMetadata::from_settings(
            &step_context.settings,
        ))
    {
        request_meta.insert(
            crate::X_CODEX_TURN_METADATA_HEADER.to_string(),
            turn_metadata,
        );
    }

    Some(serde_json::Value::Object(request_meta))
}

fn with_mcp_tool_call_ids_meta(
    meta: Option<serde_json::Value>,
    thread_id: &str,
    session_id: &str,
    originating_call: &ToolCallOrigin,
) -> Option<serde_json::Value> {
    let mut map = match meta {
        Some(serde_json::Value::Object(map)) => map,
        None => serde_json::Map::new(),
        other => return other,
    };
    map.insert(
        MCP_TOOL_THREAD_ID_META_KEY.to_string(),
        serde_json::Value::String(thread_id.to_string()),
    );
    map.insert(
        MCP_TOOL_SESSION_ID_META_KEY.to_string(),
        serde_json::Value::String(session_id.to_string()),
    );
    map.insert(
        MCP_TOOL_WINDOW_ID_META_KEY.to_string(),
        serde_json::Value::String(originating_call.window_id.clone()),
    );
    if let Some(item_id) = originating_call.item_id.as_ref() {
        map.insert(
            MCP_TOOL_ITEM_ID_META_KEY.to_string(),
            serde_json::Value::String(item_id.to_string()),
        );
    }
    Some(serde_json::Value::Object(map))
}

fn mcp_tool_metadata(tool_info: &ToolInfo) -> McpToolMetadata {
    let mcp_app_resource_uri = get_mcp_app_resource_uri(tool_info.tool.meta.as_deref());
    let mcp_app_ui = mcp_app_resource_uri.as_ref().and_then(|resource_uri| {
        let preferred_model_display_mode = match tool_info
            .tool
            .meta
            .as_ref()
            .and_then(|meta| meta.get("openai/ui"))
            .and_then(|ui| ui.get("preferredModelDisplayMode"))
            .and_then(serde_json::Value::as_str)
        {
            Some("inline") => McpAppDisplayMode::Inline,
            Some("fullscreen") => McpAppDisplayMode::Fullscreen,
            _ => return None,
        };
        Some(McpAppUi {
            resource_uri: resource_uri.clone(),
            preferred_model_display_mode,
        })
    });

    McpToolMetadata {
        annotations: tool_info.tool.annotations.clone(),
        mcp_app_resource_uri,
        mcp_app_ui,
    }
}

fn get_mcp_app_resource_uri(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    meta.and_then(|meta| {
        meta.get("ui")
            .and_then(serde_json::Value::as_object)
            .and_then(|ui| ui.get("resourceUri"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                meta.get(MCP_TOOL_UI_RESOURCE_URI_META_KEY)
                    .and_then(serde_json::Value::as_str)
            })
            .or_else(|| {
                meta.get(MCP_TOOL_OPENAI_OUTPUT_TEMPLATE_META_KEY)
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_string)
    })
}

async fn notify_mcp_tool_call_skip(
    sess: &Session,
    turn_context: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    item_metadata: McpToolCallItemMetadata,
    message: String,
) -> Result<CallToolResult, String> {
    notify_mcp_tool_call_started(
        sess,
        turn_context,
        call_id,
        invocation.clone(),
        item_metadata.clone(),
    )
    .await;

    notify_mcp_tool_call_completed(
        sess,
        turn_context,
        call_id,
        invocation,
        item_metadata,
        Duration::ZERO,
        truncate_mcp_tool_result_for_event(&Err(message.clone())),
    )
    .await;
    Err(message)
}

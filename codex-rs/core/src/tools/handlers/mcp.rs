use std::sync::Arc;
use std::time::Instant;

use crate::function_tool::FunctionCallError;
use crate::mcp_tool_call::handle_mcp_tool_call;
use crate::original_image_detail::can_request_original_image_detail;
use crate::session::session::Session;
use crate::tools::context::McpToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::flat_tool_name;
use crate::tools::hook_names::HookToolName;
use crate::tools::lifecycle::notify_tool_start;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use codex_extension_api::McpToolContext;
use codex_mcp::ToolInfo;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::mcp_tool_to_responses_api_tool;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_string::take_bytes_at_char_boundary;
use futures::future::BoxFuture;
use serde_json::Map;
use serde_json::Value;

const LEGACY_MCP_TOOL_NAME_PREFIX: &str = "mcp__";
const MCP_TOOL_NAME_DELIMITER: &str = "__";
const MAX_MCP_NAMESPACE_DESCRIPTION_BYTES: usize = 512 * 1024;

pub struct McpHandler {
    tool_info: ToolInfo,
    spec: Arc<ToolSpec>,
}

impl McpHandler {
    pub fn new(tool_info: ToolInfo) -> Result<Self, serde_json::Error> {
        Self::with_schema_max_bytes(tool_info, /*schema_max_bytes*/ None)
    }

    pub fn new_with_schema_max_bytes(
        tool_info: ToolInfo,
        schema_max_bytes: usize,
    ) -> Result<Self, serde_json::Error> {
        Self::with_schema_max_bytes(tool_info, Some(schema_max_bytes))
    }

    fn with_schema_max_bytes(
        tool_info: ToolInfo,
        schema_max_bytes: Option<usize>,
    ) -> Result<Self, serde_json::Error> {
        let spec = Arc::new(create_tool_spec(&tool_info, schema_max_bytes)?);
        Ok(Self { tool_info, spec })
    }

    pub(crate) fn tool_info(&self) -> &ToolInfo {
        &self.tool_info
    }

    fn hook_tool_name(&self) -> HookToolName {
        HookToolName::new(ensure_mcp_prefix(&join_tool_name(&self.tool_name())))
    }
}

fn join_tool_name(tool_name: &ToolName) -> String {
    match tool_name.namespace.as_deref() {
        Some(namespace) => {
            let namespace = namespace.trim_end_matches('_');
            let name = tool_name.name.trim_start_matches('_');
            format!("{namespace}{MCP_TOOL_NAME_DELIMITER}{name}")
        }
        None => tool_name.name.clone(),
    }
}

fn ensure_mcp_prefix(name: &str) -> String {
    if name.starts_with(LEGACY_MCP_TOOL_NAME_PREFIX) {
        name.to_string()
    } else {
        format!("{LEGACY_MCP_TOOL_NAME_PREFIX}{name}")
    }
}

impl ToolExecutor<ToolInvocation> for McpHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_info.canonical_tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.spec.as_ref().clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        // Correctly implemented MCP servers should tolerate parallel calls to
        // tools that advertise themselves as read-only.
        self.tool_info.supports_parallel_tool_calls
            || self
                .tool_info
                .tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                .unwrap_or(false)
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        let source_name = self.tool_info.server_name.trim();
        let source_info = (!source_name.is_empty()).then(|| ToolSearchSourceInfo {
            name: source_name.to_string(),
            description: effective_namespace_description(&self.tool_info)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string),
        });

        ToolSearchInfo::from_shared_spec(
            build_mcp_search_text(&self.tool_info),
            Arc::clone(&self.spec),
            source_info,
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl McpHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let prepared_mcp_call = invocation.session.prepare_mcp_call(&self.tool_info).await;
        let mcp_tool = prepared_mcp_call.as_ref().map(|call| {
            McpToolContext::from_prepared_call(
                call,
                invocation.turn.config.mcp_servers.get(call.server_name()),
            )
        });
        notify_tool_start(&invocation, mcp_tool.as_ref()).await;

        let originating_call = invocation.originating_call().await;
        let ToolInvocation {
            session,
            step_context,
            call_id,
            payload,
            ..
        } = invocation;

        let payload = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "mcp handler received unsupported payload".to_string(),
                ));
            }
        };

        // Capture presentation policy from the same config snapshot used for execution.
        let truncation_policy = prepared_mcp_call
            .as_ref()
            .and_then(codex_mcp::PreparedMcpCall::output_token_limit)
            .map(TruncationPolicy::Tokens)
            .unwrap_or(step_context.settings.model_info.truncation_policy.into());
        let started = Instant::now();
        let result = handle_mcp_tool_call(
            Arc::clone(&session),
            &step_context,
            call_id.clone(),
            originating_call,
            &self.tool_info,
            prepared_mcp_call,
            payload,
        )
        .await;

        Ok(boxed_tool_output(McpToolOutput {
            result: result.result,
            tool_input: result.tool_input,
            wall_time: started.elapsed(),
            original_image_detail_supported: can_request_original_image_detail(
                &step_context.settings.model_info,
            ),
            truncation_policy,
        }))
    }
}

impl CoreToolRuntime for McpHandler {
    fn immutable_spec(&self) -> Option<&Arc<ToolSpec>> {
        Some(&self.spec)
    }

    fn wait_until_ready<'a>(&'a self, session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        Some(Box::pin(async move {
            session
                .wait_for_mcp_server(&self.tool_info.server_name)
                .await;
        }))
    }

    fn mcp_server_name(&self) -> Option<&str> {
        Some(&self.tool_info.server_name)
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        Some(PreToolUsePayload {
            tool_name: self.hook_tool_name(),
            tool_input: mcp_hook_tool_input(arguments),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        invocation.payload = match invocation.payload {
            ToolPayload::Function { .. } => ToolPayload::Function {
                arguments: serde_json::to_string(&updated_input).map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to serialize rewritten MCP arguments: {err}"
                    ))
                })?,
            },
            payload => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "tool {} does not support hook input rewriting for payload {payload:?}",
                    self.tool_name()
                )));
            }
        };
        Ok(invocation)
    }
    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { .. } = &invocation.payload else {
            return None;
        };

        let tool_response =
            result.post_tool_use_response(&invocation.call_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: self.hook_tool_name(),
            tool_use_id: invocation.call_id.clone(),
            tool_input: result.post_tool_use_input(&invocation.payload)?,
            tool_response,
        })
    }
}

fn create_tool_spec(
    tool_info: &ToolInfo,
    schema_max_bytes: Option<usize>,
) -> Result<ToolSpec, serde_json::Error> {
    let tool_name = tool_info.canonical_tool_name();
    let tool = mcp_tool_to_responses_api_tool(&tool_name, &tool_info.tool, schema_max_bytes)?;
    let description = effective_namespace_description(tool_info)
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(str::to_string)
        .unwrap_or_default();

    Ok(ToolSpec::Namespace(ResponsesApiNamespace {
        name: tool_info.callable_namespace.clone(),
        description: take_bytes_at_char_boundary(&description, MAX_MCP_NAMESPACE_DESCRIPTION_BYTES)
            .to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(tool)],
    }))
}

fn effective_namespace_description(tool_info: &ToolInfo) -> Option<&str> {
    tool_info.namespace_description.as_deref()
}

fn mcp_hook_tool_input(raw_arguments: &str) -> Value {
    if raw_arguments.trim().is_empty() {
        return Value::Object(Map::new());
    }

    serde_json::from_str(raw_arguments).unwrap_or_else(|_| Value::String(raw_arguments.to_string()))
}

fn build_mcp_search_text(info: &ToolInfo) -> String {
    let tool_name = info.canonical_tool_name();
    let mut schema_properties = info
        .tool
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    schema_properties.sort();
    let mut parts = vec![
        flat_tool_name(&tool_name).into_owned(),
        info.callable_name.clone(),
        info.tool.name.to_string(),
        info.server_name.clone(),
    ];
    if let Some(title) = info.tool.title.as_deref().map(str::trim)
        && !title.is_empty()
    {
        parts.push(title.to_string());
    }
    if let Some(description) = info.tool.description.as_deref().map(str::trim)
        && !description.is_empty()
    {
        parts.push(description.to_string());
    }
    if let Some(namespace_description) = effective_namespace_description(info).map(str::trim)
        && !namespace_description.is_empty()
    {
        parts.push(namespace_description.to_string());
    }
    parts.extend(schema_properties);
    parts.join(" ")
}

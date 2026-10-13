pub(crate) mod apply_patch;
pub(crate) mod apply_patch_spec;
mod current_time;
mod dynamic;
pub(crate) mod extension_tools;
mod get_context_remaining;
pub(crate) mod get_context_remaining_spec;
mod mcp;
mod mcp_resource;
pub(crate) mod mcp_resource_spec;
pub(crate) mod multi_agents;
pub(crate) mod multi_agents_common;
pub(crate) mod multi_agents_spec;
pub(crate) mod multi_agents_v2;
mod new_context_window;
pub(crate) mod new_context_window_spec;
mod plan;
pub(crate) mod plan_spec;
mod request_user_input;
mod request_user_input_async;
pub(crate) mod request_user_input_spec;
mod send_message_to_user_async;
pub(crate) mod shell_spec;
mod sleep;
mod test_sync;
pub(crate) mod test_sync_spec;
mod tool_search;
pub(crate) mod tool_search_spec;
pub(crate) mod unified_exec;
mod view_image;
pub(crate) mod view_image_spec;

use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnEnvironment;
pub use apply_patch::ApplyPatchHandler;
pub use current_time::CurrentTimeHandler;
pub use dynamic::DynamicToolHandler;
pub use get_context_remaining::GetContextRemainingHandler;
pub use mcp::McpHandler;
pub use mcp_resource::ListMcpResourceTemplatesHandler;
pub use mcp_resource::ListMcpResourcesHandler;
pub use mcp_resource::ReadMcpResourceHandler;
pub use new_context_window::NewContextWindowHandler;
pub use plan::PlanHandler;
pub use request_user_input::RequestUserInputHandler;
pub use request_user_input_async::RequestUserInputAsyncHandler;
pub use send_message_to_user_async::SendMessageToUserAsyncHandler;
pub use sleep::SleepHandler;
pub use test_sync::TestSyncHandler;
pub(crate) use tool_search::ToolSearchHandlerCache;
pub use unified_exec::ExecCommandHandler;
pub(crate) use unified_exec::ExecCommandHandlerOptions;
pub use unified_exec::WriteStdinHandler;
pub use view_image::ViewImageHandler;

pub(crate) fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

fn updated_hook_command(updated_input: &Value) -> Result<&str, FunctionCallError> {
    updated_input
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "hook returned updatedInput without string field `command`".to_string(),
            )
        })
}

fn rewrite_function_arguments(
    arguments: &str,
    tool_name: &str,
    rewrite: impl FnOnce(&mut Map<String, Value>),
) -> Result<String, FunctionCallError> {
    let mut arguments: Value = parse_arguments(arguments)?;
    let Value::Object(arguments) = &mut arguments else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} arguments must be an object"
        )));
    };
    rewrite(arguments);
    serde_json::to_string(&arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize rewritten {tool_name} arguments: {err}"
        ))
    })
}

fn rewrite_function_string_argument(
    arguments: &str,
    tool_name: &str,
    field_name: &str,
    value: &str,
) -> Result<String, FunctionCallError> {
    rewrite_function_arguments(arguments, tool_name, |arguments| {
        arguments.insert(field_name.to_string(), Value::String(value.to_string()));
    })
}

fn resolve_tool_environment<'a>(
    step_context: &'a StepContext,
    unavailable_message: &'static str,
) -> Result<&'a TurnEnvironment, FunctionCallError> {
    step_context
        .environments
        .primary()
        .ok_or_else(|| FunctionCallError::RespondToModel(unavailable_message.to_string()))
}

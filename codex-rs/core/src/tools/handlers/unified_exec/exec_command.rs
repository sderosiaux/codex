use std::sync::Arc;

use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::rewrite_function_string_argument;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecProcessManager;
use codex_features::Feature;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_string::truncate_middle_chars;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_exec_command_tool;
use super::ExecCommandArgs;
use super::ExecCommandEnvironmentArgs;
use super::get_command;
use super::post_unified_exec_tool_use_payload;

// A byte limit is a conservative hard token bound even for byte-fallback tokenizers.
const EXEC_COMMAND_REJECTION_MAX_BYTES: usize = 900;

#[derive(Clone, Copy)]
pub(crate) struct ExecCommandHandlerOptions {
    pub(crate) include_login_parameter: bool,
    pub(crate) include_shell_parameter: bool,
    pub(crate) allow_tty: bool,
    pub(crate) include_windows_shell_guidance: bool,
}

pub struct ExecCommandHandler {
    options: ExecCommandHandlerOptions,
}

impl Default for ExecCommandHandler {
    fn default() -> Self {
        Self {
            options: ExecCommandHandlerOptions {
                include_login_parameter: false,
                include_shell_parameter: true,
                allow_tty: true,
                include_windows_shell_guidance: cfg!(windows),
            },
        }
    }
}

impl ExecCommandHandler {
    pub(crate) fn new(options: ExecCommandHandlerOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for ExecCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exec_command")
    }

    fn spec(&self) -> ToolSpec {
        let spec = create_exec_command_tool(
            CommandToolOptions {
                include_login_parameter: self.options.include_login_parameter,
            },
            self.options.include_shell_parameter,
            self.options.include_windows_shell_guidance,
        );
        let mut spec = spec;
        if !self.options.allow_tty
            && let ToolSpec::Function(spec) = &mut spec
        {
            spec.parameters
                .properties
                .get_or_insert_default()
                .remove("tty");
        }
        spec
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ExecCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "exec_command handler received unsupported payload".to_string(),
                ));
            }
        };

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::new(
            session.clone(),
            step_context.clone(),
            cancellation_token,
            call_id.clone(),
        );
        let environment_args: ExecCommandEnvironmentArgs = parse_arguments(&arguments)?;
        let turn_environment =
            resolve_tool_environment(&step_context, "unified exec is unavailable in this session")?;
        let native_environment_cwd = turn_environment.cwd().clone();
        let cwd = environment_args
            .workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map_or_else(
                || Ok(native_environment_cwd.clone()),
                |workdir| native_environment_cwd.join(workdir),
            )
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let environment = Arc::clone(&turn_environment.environment);
        let fs = environment.get_filesystem();

        // `to_abs_path()` alone cannot identify foreign drive paths: `file:///C:/repo` is
        // representable as `/C:/repo` on POSIX. Only native cwd values are host paths.
        let native_cwd = cwd.to_abs_path().ok().filter(|_| {
            cwd.infer_path_convention() == Some(codex_utils_path_uri::PathConvention::native())
        });
        let args: ExecCommandArgs = parse_arguments(&arguments)?;
        if args.tty && !session.features().enabled(Feature::UnifiedExecTty) {
            return Err(FunctionCallError::RespondToModel(
                "TTY execution is disabled by config; omit `tty` or set it to false.".to_string(),
            ));
        }
        let hook_command = args.cmd.clone();
        maybe_emit_implicit_skill_invocation(
            session.as_ref(),
            context.step_context.turn.as_ref(),
            &hook_command,
            native_cwd.as_ref(),
        )
        .await;
        let shell = turn_environment
            .shell
            .clone()
            .map(Arc::new)
            .unwrap_or_else(|| session.user_shell());
        let resolved_command =
            get_command(&args, shell, turn_environment.config().allow_login_shell)
                .map_err(FunctionCallError::RespondToModel)?;
        let command = resolved_command.command;
        let ExecCommandArgs {
            tty,
            yield_time_ms,
            max_output_tokens,
            ..
        } = args;

        let intercepted_patch = intercept_apply_patch(
            &command,
            &cwd,
            fs.as_ref(),
            turn_environment.clone(),
            context.session.clone(),
            Arc::clone(&context.step_context),
            Some(&tracker),
            &context.call_id,
        )
        .await;
        if let Some(output) = intercepted_patch? {
            return Ok(boxed_tool_output(ExecCommandToolOutput {
                event_call_id: String::new(),
                chunk_id: String::new(),
                wall_time: std::time::Duration::ZERO,
                raw_output: output.into_text().into_bytes(),
                truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                max_output_tokens,
                process_id: None,
                exit_code: None,
                original_token_count: None,
                output_omitted_bytes: None,
                hook_command: None,
            }));
        }

        crate::tools::lifecycle::notify_command_start(
            context.session.as_ref(),
            context.step_context.turn.as_ref(),
            &context.call_id,
            &command,
            &cwd,
            fs.as_ref(),
        )
        .await;
        // Preparation can be cancelled. Reserve a process only once it is done.
        let process_id = manager.allocate_process_id().await;
        let request = ExecCommandRequest {
            command,
            shell: resolved_command.shell,
            hook_command: hook_command.clone(),
            process_id,
            yield_time_ms,
            max_output_tokens,
            cwd,
            turn_environment: turn_environment.clone(),
            tty,
        };
        match manager.exec_command(request, &context).await {
            Ok(response) => Ok(boxed_tool_output(response)),
            Err(err) => {
                let message = format!("exec_command failed: {err:?}");
                Err(FunctionCallError::RespondToModel(truncate_middle_chars(
                    &message,
                    EXEC_COMMAND_REJECTION_MAX_BYTES,
                )))
            }
        }
    }
}

impl CoreToolRuntime for ExecCommandHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        parse_arguments::<ExecCommandArgs>(arguments)
            .ok()
            .map(|args| PreToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_input: serde_json::json!({ "command": args.cmd }),
            })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported exec_command payload".to_string(),
            ));
        };
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_string_argument(
                &arguments,
                "exec_command",
                "cmd",
                updated_hook_command(&updated_input)?,
            )?,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

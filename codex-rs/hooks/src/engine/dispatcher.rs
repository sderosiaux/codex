use std::path::Path;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::Map;
use serde_json::Value;

use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookScope;

use super::ClaudeHooksEngine;
use super::ConfiguredHandler;
use super::ConfiguredHandlerKind;
use super::HandlerRunResult;
use super::HandlerSourcePath;
use super::command_runner::run_command;
use super::mcp_runner::run_mcp_tool;
use crate::events::common::matches_matcher;

#[derive(Debug)]
pub(crate) struct ParsedHandler<T> {
    pub completed: HookCompletedEvent,
    pub data: T,
    pub completion_order: usize,
}

pub(crate) fn select_handlers(
    handlers: &[ConfiguredHandler],
    event_name: HookEventName,
    matcher_input: Option<&str>,
) -> Vec<ConfiguredHandler> {
    let matcher_inputs = matcher_input.into_iter().collect::<Vec<_>>();
    select_handlers_for_matcher_inputs(handlers, event_name, &matcher_inputs)
}

pub(crate) fn select_handlers_for_matcher_inputs(
    handlers: &[ConfiguredHandler],
    event_name: HookEventName,
    matcher_inputs: &[&str],
) -> Vec<ConfiguredHandler> {
    // Check each configured handler once, even when several compatibility names
    // match the same regex. A hook like `apply_patch|Write|Edit` should run a
    // single time for one tool call, not once per matching alias.
    handlers
        .iter()
        .filter(|handler| handler.event_name == event_name)
        .filter(|handler| match event_name {
            HookEventName::PreToolUse
            | HookEventName::PostToolUse
            | HookEventName::SessionStart
            | HookEventName::SessionEnd
            | HookEventName::SubagentStart
            | HookEventName::SubagentStop
            | HookEventName::PreCompact
            | HookEventName::PostCompact => {
                if matcher_inputs.is_empty() {
                    matches_matcher(handler.matcher.as_ref(), /*input*/ None)
                } else {
                    matcher_inputs
                        .iter()
                        .any(|input| matches_matcher(handler.matcher.as_ref(), Some(input)))
                }
            }
            HookEventName::UserPromptSubmit | HookEventName::Stop | HookEventName::Interrupt => {
                true
            }
        })
        .cloned()
        .collect()
}

pub(crate) fn running_summary(handler: &ConfiguredHandler) -> HookRunSummary {
    let HandlerSourcePath::Local(source_path) = &handler.source_path else {
        unreachable!("executor-scoped hooks do not produce public hook summaries");
    };
    HookRunSummary {
        builtin: handler.builtin,
        id: handler.run_id(),
        event_name: handler.event_name,
        handler_type: handler.handler_type(),
        execution_mode: handler.execution_mode(),
        scope: scope_for_event(handler.event_name),
        source_path: source_path.clone(),
        source: handler.source,
        display_order: handler.display_order,
        status: HookRunStatus::Running,
        status_message: handler.status_message.clone(),
        started_at: chrono::Utc::now().timestamp(),
        completed_at: None,
        duration_ms: None,
        entries: Vec::new(),
    }
}

pub(crate) async fn execute_handlers<T: 'static>(
    engine: &ClaudeHooksEngine,
    handlers: Vec<ConfiguredHandler>,
    input_json: String,
    cwd: &Path,
    turn_id: Option<String>,
    parse: fn(&ConfiguredHandler, HandlerRunResult, Option<String>) -> ParsedHandler<T>,
) -> Vec<ParsedHandler<T>> {
    execute_handlers_with_metadata(
        engine, handlers, input_json, cwd, turn_id, /*metadata*/ None, parse,
    )
    .await
}

pub(crate) async fn execute_handlers_with_metadata<T: 'static>(
    engine: &ClaudeHooksEngine,
    handlers: Vec<ConfiguredHandler>,
    input_json: String,
    cwd: &Path,
    turn_id: Option<String>,
    metadata: Option<&Map<String, Value>>,
    parse: fn(&ConfiguredHandler, HandlerRunResult, Option<String>) -> ParsedHandler<T>,
) -> Vec<ParsedHandler<T>> {
    let mut executor_handlers = Vec::new();
    let mut pending = FuturesUnordered::new();
    for (configured_order, handler) in handlers.into_iter().enumerate() {
        if matches!(
            handler.source_path,
            HandlerSourcePath::ExecutorScoped { .. }
        ) {
            executor_handlers.push(handler);
            continue;
        }
        if handler.execution_mode() == HookExecutionMode::Async {
            engine.command_runtime.schedule_async_hook(
                handler,
                input_json.clone(),
                cwd.to_path_buf(),
                turn_id.clone(),
                parse,
            );
            continue;
        }
        let input_json = input_json.clone();
        let turn_id = turn_id.clone();
        pending.push(async move {
            let result =
                execute_handler(engine, &handler, &input_json, cwd, /*metadata*/ None).await;
            (configured_order, parse(&handler, result, turn_id))
        });
    }

    let mut completed = Vec::new();
    let mut completion_order = 0;
    let mut should_stop = false;
    let mut should_block = false;
    while let Some((configured_order, mut parsed)) = pending.next().await {
        should_stop |= parsed.completed.run.status == HookRunStatus::Stopped;
        should_block |= parsed.completed.run.status == HookRunStatus::Blocked;
        parsed.completion_order = completion_order;
        completion_order += 1;
        completed.push((configured_order, parsed));
    }
    completed.sort_by_key(|(configured_order, _)| *configured_order);

    if should_stop || !should_block {
        for handler in executor_handlers {
            let task_engine = engine.clone();
            let input_json = input_json.clone();
            let cwd = cwd.to_path_buf();
            let metadata = metadata.cloned();
            engine.command_runtime.schedule_async_task(async move {
                let result =
                    execute_handler(&task_engine, &handler, &input_json, &cwd, metadata.as_ref())
                        .await;
                if let Some(error) = result.error {
                    tracing::warn!(
                        source_path = %handler.source_path,
                        %error,
                        "executor-scoped hook failed"
                    );
                }
            });
        }
    }

    completed.into_iter().map(|(_, parsed)| parsed).collect()
}

async fn execute_handler(
    engine: &ClaudeHooksEngine,
    handler: &ConfiguredHandler,
    input_json: &str,
    cwd: &Path,
    metadata: Option<&Map<String, Value>>,
) -> HandlerRunResult {
    match &handler.kind {
        ConfiguredHandlerKind::Command { command, env, .. } => {
            run_command(
                &engine.command_runtime,
                handler,
                command,
                env,
                input_json,
                cwd,
            )
            .await
        }
        ConfiguredHandlerKind::McpTool {
            server,
            tool,
            input,
        } => {
            run_mcp_tool(
                engine.mcp_executor.as_ref(),
                handler,
                server,
                tool,
                input,
                input_json,
                metadata,
            )
            .await
        }
    }
}

pub(crate) fn completed_summary(
    handler: &ConfiguredHandler,
    run_result: &HandlerRunResult,
    status: HookRunStatus,
    entries: Vec<codex_protocol::protocol::HookOutputEntry>,
) -> HookRunSummary {
    let HandlerSourcePath::Local(source_path) = &handler.source_path else {
        unreachable!("executor-scoped hooks do not produce public hook summaries");
    };
    HookRunSummary {
        builtin: handler.builtin,
        id: handler.run_id(),
        event_name: handler.event_name,
        handler_type: handler.handler_type(),
        execution_mode: handler.execution_mode(),
        scope: scope_for_event(handler.event_name),
        source_path: source_path.clone(),
        source: handler.source,
        display_order: handler.display_order,
        status,
        status_message: handler.status_message.clone(),
        started_at: run_result.started_at,
        completed_at: Some(run_result.completed_at),
        duration_ms: Some(run_result.duration_ms),
        entries,
    }
}

pub(crate) fn scope_for_event(event_name: HookEventName) -> HookScope {
    match event_name {
        HookEventName::SessionStart | HookEventName::SessionEnd | HookEventName::SubagentStart => {
            HookScope::Thread
        }
        HookEventName::PreToolUse
        | HookEventName::PostToolUse
        | HookEventName::PreCompact
        | HookEventName::PostCompact
        | HookEventName::UserPromptSubmit
        | HookEventName::SubagentStop
        | HookEventName::Stop
        | HookEventName::Interrupt => HookScope::Turn,
    }
}

pub(crate) fn hook_event_name_label(event_name: HookEventName) -> &'static str {
    match event_name {
        HookEventName::PreToolUse => "PreToolUse",
        HookEventName::PostToolUse => "PostToolUse",
        HookEventName::PreCompact => "PreCompact",
        HookEventName::PostCompact => "PostCompact",
        HookEventName::SessionStart => "SessionStart",
        HookEventName::SessionEnd => "SessionEnd",
        HookEventName::UserPromptSubmit => "UserPromptSubmit",
        HookEventName::SubagentStart => "SubagentStart",
        HookEventName::SubagentStop => "SubagentStop",
        HookEventName::Stop => "Stop",
        HookEventName::Interrupt => "Interrupt",
    }
}

/// Returns the canonical label for a hook execution mode.
pub fn hook_execution_mode_label(mode: HookExecutionMode) -> &'static str {
    match mode {
        HookExecutionMode::Sync => "sync",
        HookExecutionMode::Async => "async",
    }
}

/// Returns the canonical label for a hook handler type.
pub fn hook_handler_type_label(handler_type: HookHandlerType) -> &'static str {
    match handler_type {
        HookHandlerType::Command => "command",
        HookHandlerType::McpTool => "mcp_tool",
        HookHandlerType::Prompt => "prompt",
        HookHandlerType::Agent => "agent",
    }
}

pub(crate) fn hook_scope_label(scope: HookScope) -> &'static str {
    match scope {
        HookScope::Thread => "thread",
        HookScope::Turn => "turn",
    }
}

pub(crate) fn hook_source_label(source: codex_protocol::protocol::HookSource) -> &'static str {
    match source {
        codex_protocol::protocol::HookSource::System => "system",
        codex_protocol::protocol::HookSource::User => "user",
        codex_protocol::protocol::HookSource::Project => "project",
        codex_protocol::protocol::HookSource::SessionFlags => "session_flags",
        codex_protocol::protocol::HookSource::Plugin => "plugin",
        codex_protocol::protocol::HookSource::Unknown => "unknown",
    }
}

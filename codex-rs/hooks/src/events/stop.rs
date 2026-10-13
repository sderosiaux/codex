use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::items::HookPromptFragment;
use codex_protocol::protocol::HookCompletedEvent;
use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookOutputEntry;
use codex_protocol::protocol::HookOutputEntryKind;
use codex_protocol::protocol::HookRunStatus;
use codex_protocol::protocol::HookRunSummary;
use codex_protocol::protocol::HookSource;
use codex_utils_absolute_path::AbsolutePathBuf;

use super::common;
use crate::engine::ClaudeHooksEngine;
use crate::engine::ConfiguredHandler;
use crate::engine::HandlerRunResult;
use crate::engine::dispatcher;
use crate::engine::output_parser;
use crate::schema::NullableString;
use crate::schema::StopCommandInput;
use crate::schema::SubagentStopCommandInput;

#[derive(Debug, Clone)]
pub struct StopRequest {
    pub session_id: ThreadId,
    pub turn_id: String,
    pub cwd: AbsolutePathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
    pub permission_mode: String,
    pub stop_hook_active: bool,
    pub last_assistant_message: Option<String>,
    pub target: StopHookTarget,
}

#[derive(Debug, Clone)]
pub enum StopHookTarget {
    Stop,
    /// Internal memory work runs policy and executor hooks, not project completion checks.
    MemoryConsolidation,
    SubagentStop {
        agent_id: String,
        agent_type: String,
        agent_transcript_path: Option<PathBuf>,
    },
}

impl StopHookTarget {
    fn event_name(&self) -> HookEventName {
        match self {
            Self::Stop | Self::MemoryConsolidation => HookEventName::Stop,
            Self::SubagentStop { .. } => HookEventName::SubagentStop,
        }
    }

    fn matcher_input(&self) -> Option<&str> {
        match self {
            Self::Stop | Self::MemoryConsolidation => None,
            Self::SubagentStop { agent_type, .. } => Some(agent_type.as_str()),
        }
    }

    fn select_handlers(&self, handlers: &[ConfiguredHandler]) -> Vec<ConfiguredHandler> {
        dispatcher::select_handlers(handlers, self.event_name(), self.matcher_input())
            .into_iter()
            .filter(|handler| {
                !matches!(self, Self::MemoryConsolidation)
                    || match handler.source {
                        HookSource::User | HookSource::Project | HookSource::SessionFlags => false,
                        HookSource::System | HookSource::Unknown => true,
                    }
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct StopOutcome {
    pub hook_events: Vec<HookCompletedEvent>,
    pub should_stop: bool,
    pub stop_reason: Option<String>,
    pub should_block: bool,
    pub block_reason: Option<String>,
    pub continuation_fragments: Vec<HookPromptFragment>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StopHandlerData {
    should_stop: bool,
    stop_reason: Option<String>,
    should_block: bool,
    block_reason: Option<String>,
    continuation_fragments: Vec<HookPromptFragment>,
}

pub(crate) fn preview(
    handlers: &[ConfiguredHandler],
    request: &StopRequest,
) -> Vec<HookRunSummary> {
    request
        .target
        .select_handlers(handlers)
        .into_iter()
        .map(|handler| dispatcher::running_summary(&handler))
        .collect()
}

pub(crate) async fn run(engine: &ClaudeHooksEngine, request: StopRequest) -> StopOutcome {
    let matched = request.target.select_handlers(&engine.handlers);
    if matched.is_empty() {
        return StopOutcome {
            hook_events: Vec::new(),
            should_stop: false,
            stop_reason: None,
            should_block: false,
            block_reason: None,
            continuation_fragments: Vec::new(),
        };
    }

    let input_json = match request.target {
        StopHookTarget::Stop | StopHookTarget::MemoryConsolidation => {
            let input = StopCommandInput {
                session_id: request.session_id.to_string(),
                turn_id: request.turn_id.clone(),
                transcript_path: NullableString::from_path(request.transcript_path.clone()),
                cwd: request.cwd.display().to_string(),
                hook_event_name: "Stop".to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                stop_hook_active: request.stop_hook_active,
                last_assistant_message: NullableString::from_string(
                    request.last_assistant_message.clone(),
                ),
            };
            match serde_json::to_string(&input) {
                Ok(input_json) => input_json,
                Err(error) => {
                    return serialization_failure_outcome(
                        common::serialization_failure_hook_events(
                            matched,
                            Some(request.turn_id),
                            format!("failed to serialize stop hook input: {error}"),
                        ),
                    );
                }
            }
        }
        StopHookTarget::SubagentStop {
            agent_id,
            agent_type,
            agent_transcript_path,
        } => {
            let input = SubagentStopCommandInput {
                session_id: request.session_id.to_string(),
                turn_id: request.turn_id.clone(),
                transcript_path: NullableString::from_path(request.transcript_path.clone()),
                agent_transcript_path: NullableString::from_path(agent_transcript_path),
                cwd: request.cwd.display().to_string(),
                hook_event_name: "SubagentStop".to_string(),
                model: request.model.clone(),
                permission_mode: request.permission_mode.clone(),
                stop_hook_active: request.stop_hook_active,
                agent_id,
                agent_type,
                last_assistant_message: NullableString::from_string(
                    request.last_assistant_message.clone(),
                ),
            };
            match serde_json::to_string(&input) {
                Ok(input_json) => input_json,
                Err(error) => {
                    return serialization_failure_outcome(
                        common::serialization_failure_hook_events(
                            matched,
                            Some(request.turn_id),
                            format!("failed to serialize subagent stop hook input: {error}"),
                        ),
                    );
                }
            }
        }
    };

    let results = dispatcher::execute_handlers(
        engine,
        matched,
        input_json,
        request.cwd.as_path(),
        Some(request.turn_id.clone()),
        parse_completed,
    )
    .await;

    let aggregate = aggregate_results(results.iter().map(|result| &result.data));

    StopOutcome {
        hook_events: results.into_iter().map(|result| result.completed).collect(),
        should_stop: aggregate.should_stop,
        stop_reason: aggregate.stop_reason,
        should_block: aggregate.should_block,
        block_reason: aggregate.block_reason,
        continuation_fragments: aggregate.continuation_fragments,
    }
}

fn parse_completed(
    handler: &ConfiguredHandler,
    run_result: HandlerRunResult,
    turn_id: Option<String>,
) -> dispatcher::ParsedHandler<StopHandlerData> {
    let mut entries = Vec::new();
    let mut status = HookRunStatus::Completed;
    let mut should_stop = false;
    let mut stop_reason = None;
    let mut should_block = false;
    let mut block_reason = None;
    let mut continuation_prompt = None;
    let hook_event_name = match handler.event_name {
        HookEventName::Stop | HookEventName::SubagentStop => handler.event_name,
        event_name => {
            panic!("expected stop hook event, got {event_name:?}");
        }
    };

    match run_result.error.as_deref() {
        Some(error) => {
            status = HookRunStatus::Failed;
            entries.push(HookOutputEntry {
                kind: HookOutputEntryKind::Error,
                text: error.to_string(),
            });
        }
        None => match run_result.exit_code {
            Some(0) => {
                let trimmed_stdout = run_result.stdout.trim();
                if trimmed_stdout.is_empty() {
                } else if let Some(parsed) = match hook_event_name {
                    HookEventName::Stop => output_parser::parse_stop(&run_result.stdout),
                    HookEventName::SubagentStop => {
                        output_parser::parse_subagent_stop(&run_result.stdout)
                    }
                    _ => unreachable!("validated stop hook event"),
                } {
                    if let Some(system_message) = parsed.universal.system_message {
                        entries.push(HookOutputEntry {
                            kind: HookOutputEntryKind::Warning,
                            text: system_message,
                        });
                    }
                    let _ = parsed.universal.suppress_output;
                    if handler.can_apply_control_effects() {
                        if !parsed.universal.continue_processing {
                            status = HookRunStatus::Stopped;
                            should_stop = true;
                            stop_reason = parsed.universal.stop_reason.clone();
                            if let Some(stop_reason_text) = parsed.universal.stop_reason {
                                entries.push(HookOutputEntry {
                                    kind: HookOutputEntryKind::Stop,
                                    text: stop_reason_text,
                                });
                            }
                        } else if let Some(invalid_block_reason) = parsed.invalid_block_reason {
                            status = HookRunStatus::Failed;
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Error,
                                text: invalid_block_reason,
                            });
                        } else if parsed.should_block
                            && let Some(reason) =
                                parsed.reason.as_deref().and_then(common::trimmed_non_empty)
                        {
                            status = HookRunStatus::Blocked;
                            should_block = true;
                            block_reason = Some(reason.clone());
                            continuation_prompt = Some(reason.clone());
                            entries.push(HookOutputEntry {
                                kind: HookOutputEntryKind::Feedback,
                                text: reason,
                            });
                        }
                    }
                } else if handler.can_apply_control_effects()
                    || output_parser::looks_like_json(&run_result.stdout)
                {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: match hook_event_name {
                            HookEventName::Stop => "hook returned invalid stop hook JSON output",
                            HookEventName::SubagentStop => {
                                "hook returned invalid subagent stop hook JSON output"
                            }
                            _ => unreachable!("validated stop hook event"),
                        }
                        .to_string(),
                    });
                }
            }
            Some(2) if handler.can_apply_control_effects() => {
                if let Some(reason) = common::trimmed_non_empty(&run_result.stderr) {
                    status = HookRunStatus::Blocked;
                    should_block = true;
                    block_reason = Some(reason.clone());
                    continuation_prompt = Some(reason.clone());
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Feedback,
                        text: reason,
                    });
                } else {
                    status = HookRunStatus::Failed;
                    entries.push(HookOutputEntry {
                        kind: HookOutputEntryKind::Error,
                        text: match hook_event_name {
                            HookEventName::Stop => {
                                "Stop hook exited with code 2 but did not write a continuation prompt to stderr"
                            }
                            HookEventName::SubagentStop => {
                                "SubagentStop hook exited with code 2 but did not write a continuation prompt to stderr"
                            }
                            _ => unreachable!("validated stop hook event"),
                        }
                        .to_string(),
                    });
                }
            }
            Some(exit_code) => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: format!("hook exited with code {exit_code}"),
                });
            }
            None => {
                status = HookRunStatus::Failed;
                entries.push(HookOutputEntry {
                    kind: HookOutputEntryKind::Error,
                    text: "hook exited without a status code".to_string(),
                });
            }
        },
    }

    let completed = HookCompletedEvent {
        turn_id,
        run: dispatcher::completed_summary(handler, &run_result, status, entries),
    };
    let continuation_fragments = continuation_prompt
        .map(|prompt| {
            vec![HookPromptFragment::from_single_hook(
                prompt,
                completed.run.id.clone(),
            )]
        })
        .unwrap_or_default();

    dispatcher::ParsedHandler {
        completed,
        data: StopHandlerData {
            should_stop,
            stop_reason,
            should_block,
            block_reason,
            continuation_fragments,
        },
        completion_order: 0,
    }
}

fn aggregate_results<'a>(
    results: impl IntoIterator<Item = &'a StopHandlerData>,
) -> StopHandlerData {
    let results = results.into_iter().collect::<Vec<_>>();
    let should_stop = results.iter().any(|result| result.should_stop);
    let stop_reason = results.iter().find_map(|result| result.stop_reason.clone());
    let should_block = !should_stop && results.iter().any(|result| result.should_block);
    let block_reason = if should_block {
        common::join_text_chunks(
            results
                .iter()
                .filter_map(|result| result.block_reason.clone())
                .collect(),
        )
    } else {
        None
    };
    let continuation_fragments = if should_block {
        results
            .iter()
            .filter(|result| result.should_block)
            .flat_map(|result| result.continuation_fragments.clone())
            .collect()
    } else {
        Vec::new()
    };

    StopHandlerData {
        should_stop,
        stop_reason,
        should_block,
        block_reason,
        continuation_fragments,
    }
}

fn serialization_failure_outcome(hook_events: Vec<HookCompletedEvent>) -> StopOutcome {
    StopOutcome {
        hook_events,
        should_stop: false,
        stop_reason: None,
        should_block: false,
        block_reason: None,
        continuation_fragments: Vec::new(),
    }
}

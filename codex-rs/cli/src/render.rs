//! Human-readable rendering of thread events: the assistant's text on stdout, tool activity on
//! stderr, so `codex "…" > answer.md` keeps only the answer.

use std::io::IsTerminal;
use std::io::Write;

use codex_core_api::EventMsg;
use codex_core_api::FileChange;
use codex_core_api::StepStatus;
use codex_core_api::TokenUsageInfo;

/// Lines of failing command output echoed back; the model already saw the full output.
const FAILED_OUTPUT_TAIL_LINES: usize = 12;

pub(crate) struct HumanRenderer {
    color: bool,
    show_reasoning: bool,
    /// The current assistant message has been (partly) streamed through deltas.
    streamed_message: bool,
    /// Stdout does not end with a newline yet.
    open_line: bool,
    token_usage: Option<TokenUsageInfo>,
}

impl HumanRenderer {
    pub(crate) fn new(show_reasoning: bool) -> Self {
        Self {
            color: std::io::stderr().is_terminal(),
            show_reasoning,
            streamed_message: false,
            open_line: false,
            token_usage: None,
        }
    }

    pub(crate) fn render(&mut self, msg: &EventMsg) {
        match msg {
            EventMsg::AgentMessageContentDelta(event) => {
                self.streamed_message = true;
                self.out(&event.delta);
            }
            EventMsg::AgentMessage(event) => {
                if !self.streamed_message {
                    self.out(&event.message);
                }
                self.streamed_message = false;
                self.end_line();
            }
            EventMsg::ReasoningContentDelta(event) if self.show_reasoning => {
                self.err_dim(&event.delta, /*newline*/ false);
            }
            EventMsg::AgentReasoningSectionBreak(_) if self.show_reasoning => {
                self.err_dim("", /*newline*/ true);
            }
            EventMsg::ExecCommandBegin(event) => {
                self.note("$", &shell_join(&event.command));
            }
            EventMsg::ExecCommandEnd(event) => {
                let seconds = event.duration.as_secs_f32();
                if event.exit_code == 0 {
                    self.err_dim(&format!("  exit 0 · {seconds:.1}s"), /*newline*/ true);
                } else {
                    self.warn(&format!("  exit {} · {seconds:.1}s", event.exit_code));
                    for line in tail_lines(&event.aggregated_output, FAILED_OUTPUT_TAIL_LINES) {
                        self.err_dim(&format!("  │ {line}"), /*newline*/ true);
                    }
                }
            }
            EventMsg::PatchApplyEnd(event) => {
                let mut paths = event
                    .changes
                    .iter()
                    .map(|(path, change)| format!("{} {}", change_marker(change), path.display()))
                    .collect::<Vec<_>>();
                paths.sort();
                if event.success {
                    self.note("patch", &paths.join(", "));
                } else {
                    self.warn(&format!("patch failed: {}", event.stderr.trim()));
                }
            }
            EventMsg::McpToolCallBegin(event) => {
                self.note(
                    "mcp",
                    &format!("{}.{}", event.invocation.server, event.invocation.tool),
                );
            }
            EventMsg::McpToolCallEnd(event) if !event.is_success() => {
                self.warn(&format!(
                    "  {}.{} failed",
                    event.invocation.server, event.invocation.tool
                ));
            }
            EventMsg::WebSearchEnd(event) => self.note("search", &event.query),
            EventMsg::ViewImageToolCall(event) => self.note("image", &event.path.to_string()),
            EventMsg::PlanUpdate(update) => {
                for item in &update.plan {
                    let marker = match item.status {
                        StepStatus::Completed => "[x]",
                        StepStatus::InProgress => "[~]",
                        StepStatus::Pending => "[ ]",
                    };
                    self.err_dim(&format!("  {marker} {}", item.step), /*newline*/ true);
                }
            }
            EventMsg::CollabAgentSpawnEnd(event) => {
                let name = event
                    .new_agent_nickname
                    .as_deref()
                    .or(event.new_agent_role.as_deref())
                    .unwrap_or("agent");
                match event.new_thread_id {
                    Some(thread_id) => self.note("spawn", &format!("{name} ({thread_id})")),
                    None => self.warn(&format!("failed to spawn {name}")),
                }
            }
            EventMsg::McpStartupComplete(event) => {
                for failure in &event.failed {
                    self.warn(&format!(
                        "MCP server `{}` failed: {}",
                        failure.server, failure.error
                    ));
                }
            }
            EventMsg::ContextCompacted(_) => self.note("compact", "context compacted"),
            EventMsg::Warning(event) => self.warn(&event.message),
            EventMsg::DeprecationNotice(event) => self.warn(&event.summary),
            EventMsg::StreamError(event) => self.warn(&event.message),
            EventMsg::Error(event) => self.error(&event.message),
            EventMsg::TokenCount(event) => {
                if let Some(info) = &event.info {
                    self.token_usage = Some(info.clone());
                }
            }
            _ => {}
        }
    }

    /// Prints the cumulative token usage once the thread is done.
    pub(crate) fn finish(&mut self) {
        self.end_line();
        if let Some(info) = &self.token_usage {
            let usage = &info.total_token_usage;
            self.err_dim(
                &format!(
                    "tokens: {} in ({} cached) · {} out",
                    usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
                ),
                /*newline*/ true,
            );
        }
    }

    fn out(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        self.open_line = !text.ends_with('\n');
    }

    fn end_line(&mut self) {
        if self.open_line {
            self.out("\n");
        }
    }

    fn note(&mut self, label: &str, text: &str) {
        self.end_line();
        let label = self.paint("36", label);
        eprintln!("{label} {text}");
    }

    fn warn(&mut self, text: &str) {
        self.end_line();
        eprintln!("{}", self.paint("33", text));
    }

    fn error(&mut self, text: &str) {
        self.end_line();
        eprintln!("{} {text}", self.paint("31", "error:"));
    }

    fn err_dim(&mut self, text: &str, newline: bool) {
        if newline {
            self.end_line();
        }
        let text = self.paint("2", text);
        if newline {
            eprintln!("{text}");
        } else {
            eprint!("{text}");
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

fn change_marker(change: &FileChange) -> &'static str {
    match change {
        FileChange::Add { .. } => "A",
        FileChange::Delete { .. } => "D",
        FileChange::Update { .. } => "M",
    }
}

/// Renders argv as a copy-pasteable shell line, unwrapping the `bash -lc '<script>'` form.
fn shell_join(command: &[String]) -> String {
    if let [shell, flag, script] = command
        && matches!(flag.as_str(), "-lc" | "-c")
        && shell
            .rsplit('/')
            .next()
            .is_some_and(|name| name.ends_with("sh"))
    {
        return script.clone();
    }
    command
        .iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn tail_lines(text: &str, count: usize) -> Vec<&str> {
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(count)..].to_vec()
}

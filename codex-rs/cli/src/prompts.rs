//! Answers for the questions a turn can ask mid-flight: MCP elicitations and the
//! `request_user_input` tool. Without a terminal every question is declined so the turn
//! never blocks on input that cannot arrive.

use std::collections::HashMap;

use codex_core_api::ElicitationAction;
use codex_core_api::ElicitationRequest;
use codex_core_api::ElicitationRequestEvent;
use codex_core_api::Op;
use codex_core_api::RequestUserInputAnswer;
use codex_core_api::RequestUserInputEvent;
use codex_core_api::RequestUserInputResponse;
use tokio::sync::mpsc;

pub(crate) struct Prompter {
    /// Terminal lines; `None` when nobody can answer.
    lines: Option<mpsc::UnboundedReceiver<String>>,
}

impl Prompter {
    pub(crate) fn non_interactive() -> Self {
        Self { lines: None }
    }

    pub(crate) fn interactive(lines: mpsc::UnboundedReceiver<String>) -> Self {
        Self { lines: Some(lines) }
    }

    /// Next terminal line, or `None` at EOF (and always when non-interactive).
    pub(crate) async fn read_line(&mut self) -> Option<String> {
        self.lines.as_mut()?.recv().await
    }

    pub(crate) async fn resolve_elicitation(&mut self, event: ElicitationRequestEvent) -> Op {
        let ElicitationRequestEvent {
            server_name,
            id,
            request,
            ..
        } = event;
        let (decision, content) = match request {
            ElicitationRequest::Url { message, url, .. } => {
                eprintln!("[{server_name}] {message}\n  open: {url}");
                if self.confirm("done? [y/N] ").await {
                    (ElicitationAction::Accept, None)
                } else {
                    (ElicitationAction::Decline, None)
                }
            }
            ElicitationRequest::Form {
                message,
                requested_schema,
                ..
            }
            | ElicitationRequest::OpenAiForm {
                message,
                requested_schema,
                ..
            }
            | ElicitationRequest::OpenAiElicitationForm {
                message,
                requested_schema,
                ..
            } => {
                eprintln!("[{server_name}] {message}\n  schema: {requested_schema}");
                self.form_response().await
            }
        };
        Op::ResolveElicitation {
            server_name,
            request_id: id,
            decision,
            content,
            meta: None,
        }
    }

    pub(crate) async fn answer_user_input(&mut self, event: RequestUserInputEvent) -> Op {
        let mut answers = HashMap::new();
        for question in event.questions {
            if self.lines.is_none() {
                eprintln!("(no terminal) skipped question: {}", question.question);
                continue;
            }
            eprintln!("{}: {}", question.header, question.question);
            let options = question.options.unwrap_or_default();
            for (index, option) in options.iter().enumerate() {
                eprintln!("  {}. {} — {}", index + 1, option.label, option.description);
            }
            eprint!("> ");
            let Some(line) = self.read_line().await else {
                break;
            };
            let answer = line
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|choice| choice.checked_sub(1))
                .and_then(|index| options.get(index))
                .map_or_else(|| line.trim().to_string(), |option| option.label.clone());
            if !answer.is_empty() {
                answers.insert(
                    question.id,
                    RequestUserInputAnswer {
                        answers: vec![answer],
                    },
                );
            }
        }
        Op::UserInputAnswer {
            id: event.turn_id,
            response: RequestUserInputResponse { answers },
        }
    }

    async fn confirm(&mut self, question: &str) -> bool {
        if self.lines.is_none() {
            eprintln!("(no terminal) declined");
            return false;
        }
        eprint!("{question}");
        self.read_line()
            .await
            .is_some_and(|line| matches!(line.trim(), "y" | "Y" | "yes"))
    }

    /// Reads a JSON object for an elicitation form; anything else declines it.
    async fn form_response(&mut self) -> (ElicitationAction, Option<serde_json::Value>) {
        if self.lines.is_none() {
            eprintln!("(no terminal) declined");
            return (ElicitationAction::Decline, None);
        }
        eprint!("JSON object to accept, empty to decline: ");
        let Some(line) = self.read_line().await else {
            return (ElicitationAction::Cancel, None);
        };
        if line.trim().is_empty() {
            return (ElicitationAction::Decline, None);
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(value @ serde_json::Value::Object(_)) => (ElicitationAction::Accept, Some(value)),
            Ok(_) | Err(_) => {
                eprintln!("not a JSON object; declined");
                (ElicitationAction::Decline, None)
            }
        }
    }
}

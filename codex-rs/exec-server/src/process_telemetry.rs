//! Emits process lifecycle telemetry and bounds process identifiers recorded by tracing.

use opentelemetry::trace::SpanContext;

/// Log fields captured at launch, never refreshed from a resumed session.
#[derive(Clone, Default)]
pub(crate) struct ProcessTelemetry {
    pub(crate) launch_context: Option<SpanContext>,
    pub(crate) thread_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
}

/// Lifecycle events with outcome fields only when a process has exited.
pub(crate) enum ProcessTelemetryEvent {
    Start,
    SpawnFailed,
    Exit {
        exit_code: i32,
        termination_requested: bool,
    },
}

impl ProcessTelemetry {
    pub(crate) fn log(&self, event: ProcessTelemetryEvent) {
        let (event_name, exit_code, termination_requested) = match event {
            ProcessTelemetryEvent::Start => ("codex.exec_server.process_start", None, None),
            ProcessTelemetryEvent::SpawnFailed => {
                ("codex.exec_server.process_spawn_failed", None, None)
            }
            ProcessTelemetryEvent::Exit {
                exit_code,
                termination_requested,
            } => (
                "codex.exec_server.process_exit",
                Some(exit_code),
                Some(termination_requested),
            ),
        };
        let trace_id = self
            .launch_context
            .as_ref()
            .map(|span| span.trace_id().to_string());
        let span_id = self
            .launch_context
            .as_ref()
            .map(|span| span.span_id().to_string());
        tracing::event!(
            target: "codex_otel.log_only",
            tracing::Level::INFO,
            event.name = event_name,
            launch.trace_id = trace_id.as_deref(),
            launch.span_id = span_id.as_deref(),
            conversation.id = self.thread_id.as_deref(),
            tool.call_id = self.tool_call_id.as_deref(),
            process.exit_code = exit_code,
            process.termination_requested = termination_requested,
        );
    }
}

pub(crate) fn trace_process_id(process_id: &str) -> Option<&str> {
    (!process_id.is_empty() && process_id.len() <= 64).then_some(process_id)
}

use std::time::Instant;

/// Tracing spans for local processes and shell-snapshot metrics on the host's global client.
#[derive(Clone, Copy, Default)]
pub(crate) struct ExecServerTelemetry;

pub(crate) struct ProcessMetricGuard {
    span: tracing::Span,
    started_at: Instant,
    result: &'static str,
}

impl ExecServerTelemetry {
    #[cfg(unix)]
    pub(crate) fn shell_snapshot_captured(
        self,
        duration: std::time::Duration,
        result: Result<(), &'static str>,
        capture_tags: &[(&str, &str)],
    ) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let success = if result.is_ok() { "true" } else { "false" };
        let mut tags = vec![("version", "v2"), ("success", success)];
        tags.extend_from_slice(capture_tags);
        if let Err(failure_reason) = result {
            tags.push(("failure_reason", failure_reason));
        }
        let _ = metrics.record_duration("codex.shell_snapshot.duration_ms", duration, &tags);
        let _ = metrics.counter("codex.shell_snapshot", /*inc*/ 1, &tags);
    }

    /// One observation per eligible execution preparation, excluding prewarm.
    /// `used` means selected for replay, not that restoration or execution succeeded.
    #[cfg(unix)]
    pub(crate) fn shell_snapshot_command(
        self,
        wait: std::time::Duration,
        state: &'static str,
        outcome: &'static str,
    ) {
        let Some(metrics) = codex_otel::global() else {
            return;
        };
        let tags = [("version", "v2"), ("state", state), ("outcome", outcome)];
        let _ = metrics.counter("codex.shell_snapshot.command", /*inc*/ 1, &tags);
        let _ = metrics.record_duration("codex.shell_snapshot.wait_ms", wait, &tags);
    }

    pub(crate) fn process_started(self, process_id: &str) -> ProcessMetricGuard {
        let parent = codex_otel::current_span_w3c_trace_context();
        // Link to the caller's trace without keeping its span alive until the process exits.
        let span = tracing::info_span!(
            parent: None,
            "codex.exec_server.process",
            otel.kind = "internal",
            process.id = process_id,
            result = tracing::field::Empty,
        );
        if let Some(parent) = parent {
            codex_otel::set_parent_from_w3c_trace_context(&span, &parent);
        }
        ProcessMetricGuard {
            span,
            started_at: Instant::now(),
            result: "unknown",
        }
    }
}

impl ProcessMetricGuard {
    pub(crate) fn finish(mut self, result: &'static str) {
        self.result = result;
    }
}

impl Drop for ProcessMetricGuard {
    fn drop(&mut self) {
        self.span.record("result", self.result);
        tracing::debug!(
            parent: &self.span,
            duration_ms = self.started_at.elapsed().as_millis() as u64,
            "process finished"
        );
    }
}

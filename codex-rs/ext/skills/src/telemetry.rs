use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Instant;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_otel::sanitize_metric_tag_value;
use codex_protocol::openai_models::ModelInfo;

use crate::state::SkillsSessionState;

/// Attributes a successful host skill invocation to its turn's latency metrics.
pub fn record_skill_turn_usage(turn_store: &ExtensionData) {
    if let Some(turn) = turn_store.get::<SkillTurnMetrics>() {
        turn.record_skill_use();
    }
}

pub(crate) struct SkillTurnMetrics {
    model_slug: String,
    pub(crate) reasoning_effort: String,
    started_at: Instant,
    usage: Mutex<Option<TurnUsage>>,
}

#[derive(Default)]
struct TurnUsage {
    used_skill: bool,
    failed: bool,
}

impl SkillTurnMetrics {
    pub(crate) fn record_skill_use(&self) {
        if let Some(usage) = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            usage.used_skill = true;
        }
    }

    fn finish(&self, session_store: &ExtensionData, status: &str) {
        let Some(usage) = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if !usage.used_skill {
            return;
        }
        let Some(metrics) = session_store
            .get::<SkillsSessionState>()
            .and_then(|state| state.extension_metrics.clone())
        else {
            return;
        };
        let status = if status == "completed" && usage.failed {
            "error"
        } else {
            status
        };
        // Seconds keep multi-minute turns within the standard histogram buckets.
        // Round up so integer-second bucket boundaries classify fractional durations correctly.
        let duration_seconds = self.started_at.elapsed().as_secs_f64().ceil() as i64;
        metrics.histogram(
            "codex.skill.turn.duration_seconds",
            duration_seconds,
            &[
                ("model_slug", self.model_slug.as_str()),
                ("reasoning_effort", self.reasoning_effort.as_str()),
                ("status", status),
            ],
        );
    }
}

#[derive(Default)]
pub(crate) struct ActiveSkillTurnMetrics(pub(crate) Mutex<Weak<SkillTurnMetrics>>);

pub(crate) struct SkillTelemetry;

impl TurnLifecycleContributor for SkillTelemetry {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let reasoning_effort = input
                .collaboration_mode
                .reasoning_effort()
                .or_else(|| {
                    input
                        .thread_store
                        .get::<ModelInfo>()
                        .and_then(|model| model.default_reasoning_level.clone())
                })
                .map(|effort| effort.to_string())
                .unwrap_or_else(|| "default".to_string());
            input.turn_store.insert(SkillTurnMetrics {
                model_slug: sanitize_metric_tag_value(input.collaboration_mode.model()),
                reasoning_effort,
                started_at: Instant::now(),
                usage: Mutex::new(Some(TurnUsage::default())),
            });
            if let Some(turn) = input.turn_store.get::<SkillTurnMetrics>() {
                *input
                    .thread_store
                    .get_or_init(ActiveSkillTurnMetrics::default)
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&turn);
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(turn) = input.turn_store.get::<SkillTurnMetrics>() {
                turn.finish(input.session_store, "completed");
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(turn) = input.turn_store.get::<SkillTurnMetrics>() {
                turn.finish(input.session_store, "aborted");
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(turn) = input.turn_store.get::<SkillTurnMetrics>()
                && let Some(usage) = turn
                    .usage
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
            {
                usage.failed = true;
            }
        })
    }
}

use crate::responses_metadata::CompactionImplementation;
use crate::responses_metadata::CompactionReason;
use crate::session::turn_context::TurnContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use tracing::warn;

/// Returns whether a failed compaction attempt should use the current model.
pub(crate) fn should_retry_with_current_model(
    error: &CodexErr,
    previous: &TurnContext,
    current: &TurnContext,
) -> bool {
    if matches!(
        error.details(),
        CodexErrorDetails::TurnAborted
            | CodexErrorDetails::Interrupted
            | CodexErrorDetails::SessionBudgetExceeded
            | CodexErrorDetails::ResponseIncomplete(_)
    ) || (previous.model_info().slug == current.model_info().slug
        && previous.cyber_access_program == current.cyber_access_program)
    {
        return false;
    }

    current.provider.info().is_openai()
        && current
            .auth_manager
            .as_deref()
            .and_then(codex_login::AuthManager::auth_cached)
            .is_some_and(|auth| auth.uses_codex_backend() || auth.is_api_key_auth())
}

pub(crate) fn record_model_fallback(
    previous_model: &str,
    current_model: &str,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    fallback_error: Option<&CodexErr>,
) {
    let outcome = if fallback_error.is_none() {
        "succeeded"
    } else {
        "failed"
    };
    warn!(
        previous_model,
        current_model,
        ?reason,
        ?implementation,
        outcome,
        ?fallback_error,
        "previous-model compaction failed; retried with current model"
    );
}

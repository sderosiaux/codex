use std::sync::Arc;

use crate::compact_retained_tool_outputs::is_retained_tool_output;

use crate::Prompt;
use crate::ResponseStream;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::build_compaction_replacement_history;
use crate::compact_model_fallback::record_model_fallback;
use crate::compact_model_fallback::should_retry_with_current_model;
use crate::compact_remote_history::HistoryItemGroup;
use crate::compact_remote_history::history_item_groups;
use crate::context::world_state::WorldState;
use crate::context_manager::estimate_item_token_count;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CompactionImplementation;
use crate::responses_metadata::CompactionPhase;
use crate::responses_metadata::CompactionReason;
use crate::responses_metadata::CompactionTrigger;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::responses_retry::ResponsesStreamRequest;
use crate::responses_retry::ResponsesStreamRetryState;
use crate::responses_retry::handle_response_stream_error;
use crate::session::RequestEffortUsage;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_context_fragments::set_annotated_content;
use codex_context_fragments::to_annotated_content;
use codex_features::Feature;
use codex_features::Features;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_output_truncation::truncate_text;
use futures::StreamExt;

#[path = "compact_remote_v2_attempt.rs"]
mod attempt;
use attempt::RemoteCompactV2Attempt;
use attempt::run_remote_compact_v2_attempt;

#[path = "compact_remote_v2_images.rs"]
mod images;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedImageBudget {
    Disabled,
    Enabled,
}

pub(crate) const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 64_000;
const MAX_RETAINED_AGENT_MESSAGE_TOKENS: i64 = 10_000;
// Compact attempts can run much longer than normal turns, so keep the per-transport
// retry budget smaller than the general Responses stream retry budget.
const MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES: u64 = 2;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    replacement_step_context: Arc<StepContext>,
    client_session: &mut ModelClientSession,
    world_state: Arc<WorldState>,
    reason: CompactionReason,
    phase: CompactionPhase,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        reason,
        CompactionImplementation::ResponsesCompactionV2,
        phase,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        &replacement_step_context,
        Some(client_session),
        world_state,
        compaction_metadata,
    )
    .await
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    world_state: Arc<WorldState>,
) -> CodexResult<()> {
    let compaction_metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Manual,
        CompactionReason::UserRequested,
        CompactionImplementation::ResponsesCompactionV2,
        CompactionPhase::StandaloneTurn,
    );
    run_remote_compact_task_inner(
        &sess,
        &step_context,
        &step_context,
        /*client_session*/ None,
        world_state,
        compaction_metadata,
    )
    .await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    replacement_step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    world_state: Arc<WorldState>,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let trigger = compaction_metadata.trigger();
    let phase = compaction_metadata.phase();
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
    }
    let result = run_remote_compact_task_inner_impl(
        sess,
        step_context,
        replacement_step_context,
        client_session,
        world_state,
        compaction_metadata,
    )
    .await;
    if result.is_ok() {
        let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
        if let PostCompactHookOutcome::Stopped = post_compact_outcome {
            return Err(CodexErr::TurnAborted);
        }
    }
    match result {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(err.details(), CodexErrorDetails::TurnAborted)
                || matches!(phase, CompactionPhase::PostTurn) =>
        {
            Err(err)
        }
        Err(err) => {
            // Pre-turn failures are reported by run_turn after preserving the incoming prompt.
            if !matches!(phase, CompactionPhase::PreTurn) {
                let event = EventMsg::Error(
                    err.to_error_event(Some("Error running remote compact task".to_string())),
                );
                sess.send_event(turn_context, event).await;
            }
            Err(err)
        }
    }
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    replacement_step_context: &Arc<StepContext>,
    mut client_session: Option<&mut ModelClientSession>,
    world_state: Arc<WorldState>,
    compaction_metadata: CompactionTurnMetadata,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let context_compaction_item = ContextCompactionItem::new();
    let compaction_item = TurnItem::ContextCompaction(context_compaction_item);
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;

    let attempt = run_remote_compact_v2_attempt(
        sess,
        step_context,
        client_session.as_deref_mut(),
        compaction_metadata,
    )
    .await;
    let (attempt, compaction_turn_context) = match attempt {
        Ok(attempt) => (attempt, turn_context),
        Err(error) => {
            let fallback_turn_context = &replacement_step_context.turn;
            if !should_retry_with_current_model(&error, turn_context, fallback_turn_context) {
                return Err(error);
            }
            sess.set_last_known_step_context(replacement_step_context)
                .await;
            let fallback_result = run_remote_compact_v2_attempt(
                sess,
                replacement_step_context,
                client_session,
                compaction_metadata,
            )
            .await;
            record_model_fallback(
                &sess.services.session_telemetry,
                turn_context.model_info().slug.as_str(),
                fallback_turn_context.model_info().slug.as_str(),
                compaction_metadata.reason(),
                compaction_metadata.implementation(),
                fallback_result.as_ref().err(),
            );
            match fallback_result {
                Ok(attempt) => (attempt, fallback_turn_context),
                Err(_) => return Err(error),
            }
        }
    };
    let RemoteCompactV2Attempt {
        input_goal_ids,
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        compaction_response_id,
        token_usage,
        owned_client_session: _owned_client_session,
    } = attempt;
    if let Some(token_usage) = token_usage {
        sess.record_rollout_budget_usage(&token_usage).await?;
    }
    let compacted_history = build_v2_compacted_history(
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        &sess.features(),
        if sess.enabled(Feature::CompactionImageBudget) {
            RetainedImageBudget::Enabled
        } else {
            RetainedImageBudget::Disabled
        },
    );
    let (new_window_number, new_window_ids) = sess.advance_auto_compact_window().await;
    let (new_history, world_state_baseline) = build_compaction_replacement_history(
        sess.as_ref(),
        replacement_step_context,
        &world_state,
        compacted_history,
    )
    .await;

    sess.replace_compacted_history(
        new_history,
        replacement_step_context.to_turn_context_item(),
        world_state_baseline,
        CompactedHistoryMetadata {
            input_goal_ids,
            message: String::new(),
            window_number: new_window_number,
            window_ids: new_window_ids,
            compaction_response_id: Some(compaction_response_id),
            compaction_model_hash: compaction_turn_context.model_info().comp_hash.clone(),
        },
    )
    .await;
    sess.recompute_token_usage(&replacement_step_context.turn)
        .await;

    sess.emit_turn_item_completed(compaction_turn_context, compaction_item)
        .await;
    Ok(())
}

struct RemoteCompactionV2Output {
    compaction_output: ResponseItem,
    response_id: String,
    token_usage: Option<TokenUsage>,
}

async fn run_remote_compaction_request_v2(
    sess: &Session,
    step_context: &StepContext,
    client_session: &mut ModelClientSession,
    prompt: &Prompt,
    responses_metadata: &CodexResponsesMetadata,
) -> CodexResult<RemoteCompactionV2Output> {
    let turn_context = &step_context.turn;
    let max_retries = turn_context
        .provider
        .info()
        .stream_max_retries()
        .min(MAX_REMOTE_COMPACTION_V2_STREAM_RETRIES);
    let mut retry_state = ResponsesStreamRetryState::default();
    loop {
        let result = match client_session
            .stream(
                prompt,
                turn_context.model_info(),
                &turn_context.session_telemetry,
                sess.reasoning_effort_for_request(
                    &turn_context.initial_settings,
                    RequestEffortUsage::Compaction,
                )
                .await,
                turn_context.reasoning_summary(),
                step_context.settings.service_tier.clone(),
                responses_metadata,
            )
            .await
        {
            Ok(stream) => collect_compaction_output(sess, turn_context, stream).await,
            Err(err) => Err(err),
        };

        match result {
            Ok(compaction_output) => return Ok(compaction_output),
            Err(err) => {
                handle_response_stream_error(
                    &mut retry_state,
                    max_retries,
                    err,
                    client_session,
                    sess,
                    step_context,
                    ResponsesStreamRequest::RemoteCompactionV2,
                )
                .await?;
            }
        }
    }
}

async fn collect_compaction_output(
    sess: &Session,
    turn_context: &TurnContext,
    mut stream: ResponseStream,
) -> CodexResult<RemoteCompactionV2Output> {
    let mut output_item_count = 0usize;
    let mut compaction_count = 0usize;
    let mut compaction_output = None;
    let mut completed_response_id = None;
    let mut completed_token_usage = None;
    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(item) => {
                output_item_count += 1;
                if let ResponseItem::Compaction { .. } = item {
                    compaction_count += 1;
                    if compaction_output.is_none() {
                        compaction_output = Some(item);
                    }
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                usage_metadata,
                ..
            } => {
                sess.record_observed_response_completed(
                    turn_context,
                    &response_id,
                    token_usage.as_ref(),
                    usage_metadata.as_ref(),
                )
                .await;
                completed_response_id = Some(response_id);
                completed_token_usage = token_usage;
                break;
            }
            _ => {}
        }
    }

    let Some(response_id) = completed_response_id else {
        return Err(CodexErr::Stream(
            "remote compaction v2 stream closed before response.completed".to_string(),
        ));
    };

    if compaction_count != 1 {
        return Err(CodexErr::Fatal(format!(
            "remote compaction v2 expected exactly one compaction output item, got {compaction_count} from {output_item_count} output items"
        )));
    }

    let Some(compaction_output) = compaction_output else {
        unreachable!("compaction output must exist when count is exactly one");
    };
    Ok(RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage: completed_token_usage,
    })
}

fn build_v2_compacted_history(
    prompt_input: Vec<ResponseItem>,
    prompt_input_metadata: Vec<Option<CodexHarnessMetadata>>,
    compaction_output: ResponseItem,
    features: &Features,
    image_budget: RetainedImageBudget,
) -> Vec<ResponseItemEnvelope> {
    debug_assert_eq!(prompt_input.len(), prompt_input_metadata.len());
    let prompt_input = prompt_input
        .into_iter()
        .zip(prompt_input_metadata)
        .map(|(item, metadata)| ResponseItemEnvelope { item, metadata })
        .collect::<Vec<_>>();
    let retained = v2_history_item_groups(prompt_input)
        .filter(|group| is_retained_for_remote_compaction_v2(&group.source, features))
        .flat_map(HistoryItemGroup::into_items)
        .collect::<Vec<_>>();
    let mut retained =
        truncate_retained_messages(retained, RETAINED_MESSAGE_TOKEN_BUDGET, image_budget);
    retained.push(ResponseItemEnvelope::new(compaction_output));
    retained
}

pub(crate) fn is_client_authored_developer_message(item: &ResponseItemEnvelope) -> bool {
    item.metadata
        .as_ref()
        .is_some_and(|metadata| metadata.client_authored)
        && matches!(&item.item, ResponseItem::Message { role, .. } if role == "developer")
}

fn v2_history_item_groups(
    items: Vec<ResponseItemEnvelope>,
) -> impl Iterator<Item = HistoryItemGroup<ResponseItemEnvelope>> {
    history_item_groups(items).flat_map(|mut group| {
        let client_message = group
            .attached_notice
            .take_if(|item| is_client_authored_developer_message(item))
            .map(|source| HistoryItemGroup {
                source,
                attached_notice: None,
            });
        std::iter::once(group).chain(client_message)
    })
}

fn is_retained_for_remote_compaction_v2(
    envelope: &ResponseItemEnvelope,
    features: &Features,
) -> bool {
    if features.enabled(Feature::RetainClientToolOutputs) && is_retained_tool_output(envelope) {
        return true;
    }
    let item = &envelope.item;
    if let ResponseItem::AgentMessage {
        author,
        recipient,
        content,
        ..
    } = item
    {
        let is_descendant_progress = author
            .strip_prefix(recipient)
            .is_some_and(|suffix| suffix.starts_with('/'))
            && matches!(
                content.first(),
                Some(AgentMessageInputContent::InputText { text })
                    if text.starts_with("Message Type: MESSAGE\n")
                        || text.starts_with("Message Type: CHANNEL_POST\n")
            );
        let is_completion = matches!(
            content.first(),
            Some(AgentMessageInputContent::InputText { text })
                if text.starts_with("Message Type: FINAL_ANSWER\n")
        );
        return !is_descendant_progress
            && !is_completion
            && estimate_item_token_count(item) <= MAX_RETAINED_AGENT_MESSAGE_TOKENS;
    }

    let ResponseItem::Message { role, .. } = item else {
        return false;
    };

    match role.as_str() {
        "user" => matches!(
            crate::event_mapping::parse_turn_item(item),
            Some(TurnItem::UserMessage(_) | TurnItem::HookPrompt(_))
        ),
        "developer" => {
            features.enabled(Feature::RetainClientDeveloperMessages)
                && is_client_authored_developer_message(envelope)
        }
        _ => false,
    }
}

fn retained_input_image_count(item: &ResponseItem) -> usize {
    if let ResponseItem::FunctionCallOutput { output, .. } = item
        && let FunctionCallOutputBody::ContentItems(content) = &output.body
    {
        return content
            .iter()
            .filter(|item| matches!(item, FunctionCallOutputContentItem::InputImage { .. }))
            .count();
    }
    let ResponseItem::Message { content, .. } = item else {
        return 0;
    };

    content
        .iter()
        .filter(|item| matches!(item, ContentItem::InputImage { .. }))
        .count()
}

pub(crate) fn truncate_retained_messages_for_remote_compaction(
    items: Vec<ResponseItemEnvelope>,
    max_tokens: usize,
) -> Vec<ResponseItemEnvelope> {
    truncate_retained_messages(items, max_tokens, RetainedImageBudget::Disabled)
}

fn truncate_retained_messages(
    items: Vec<ResponseItemEnvelope>,
    max_tokens: usize,
    image_budget: RetainedImageBudget,
) -> Vec<ResponseItemEnvelope> {
    let mut remaining = max_tokens;
    let mut truncated_reversed = Vec::with_capacity(items.len());
    for group in v2_history_item_groups(items)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if remaining == 0 {
            continue;
        }

        let client_developer = is_client_authored_developer_message(&group.source);
        let charge_images = image_budget == RetainedImageBudget::Enabled && !client_developer;
        let notice_tokens = group
            .attached_notice
            .as_ref()
            .map_or(0, |notice| message_text_token_count(&notice.item).max(1));
        // Client-authored developer messages already charge non-text content via
        // the serialized estimate. Preserve their text-only boundary correction.
        let content_tokens = if charge_images {
            message_content_token_count(&group.source.item)
        } else {
            message_text_token_count(&group.source.item)
        };
        let source_tokens = if client_developer {
            usize::try_from(estimate_item_token_count(&group.source.item)).unwrap_or(usize::MAX)
        } else {
            content_tokens.max(1)
        };
        let token_count = source_tokens.saturating_add(notice_tokens);
        if token_count <= remaining {
            if let Some(notice) = group.attached_notice {
                truncated_reversed.push(notice);
            }
            truncated_reversed.push(group.source);
            remaining = remaining.saturating_sub(token_count);
        } else if is_retained_tool_output(&group.source) {
            // Keep outputs whole and stop before older items to preserve newest-first ordering.
            break;
        } else if remaining > notice_tokens {
            let available_tokens = remaining - notice_tokens;
            let content_budget = if client_developer {
                available_tokens.saturating_sub(source_tokens.saturating_sub(content_tokens))
            } else {
                available_tokens
            };
            let image_count = retained_input_image_count(&group.source.item);
            if charge_images && image_count > 0 {
                // An oversized image can leave no boundary content. Do not backfill
                // the remaining budget with older messages in that case.
                remaining = 0;
            }
            let truncated_item = if charge_images && image_count > 0 {
                images::truncate_message_to_token_budget(group.source, content_budget)
            } else {
                truncate_message_text_to_token_budget(group.source, content_budget)
            };
            let Some(mut truncated_item) = truncated_item else {
                continue;
            };
            if client_developer {
                let item_tokens = usize::try_from(estimate_item_token_count(&truncated_item.item))
                    .unwrap_or(usize::MAX);
                if item_tokens > available_tokens {
                    let adjusted_budget = content_budget
                        .saturating_sub(item_tokens - available_tokens)
                        .saturating_sub(1);
                    let Some(adjusted) =
                        truncate_message_text_to_token_budget(truncated_item, adjusted_budget)
                    else {
                        continue;
                    };
                    if usize::try_from(estimate_item_token_count(&adjusted.item))
                        .unwrap_or(usize::MAX)
                        > available_tokens
                    {
                        continue;
                    }
                    truncated_item = adjusted;
                }
            }
            if let Some(notice) = group.attached_notice {
                truncated_reversed.push(notice);
            }
            truncated_reversed.push(truncated_item);
            remaining = 0;
        } else if charge_images && retained_input_image_count(&group.source.item) > 0 {
            remaining = 0;
        }
    }
    truncated_reversed.reverse();
    truncated_reversed
}

fn message_content_token_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return usize::try_from(estimate_item_token_count(item)).unwrap_or(usize::MAX);
    };

    content.iter().map(images::content_item_token_count).sum()
}

fn message_text_token_count(item: &ResponseItem) -> usize {
    let ResponseItem::Message { content, .. } = item else {
        return usize::try_from(estimate_item_token_count(item)).unwrap_or(usize::MAX);
    };

    content
        .iter()
        .map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                approx_token_count(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => 0,
        })
        .sum()
}

fn truncate_message_text_to_token_budget(
    mut envelope: ResponseItemEnvelope,
    max_tokens: usize,
) -> Option<ResponseItemEnvelope> {
    let content = to_annotated_content(&mut envelope.item)?;

    let mut remaining = max_tokens;
    let mut truncated_content = Vec::with_capacity(content.len());
    for mut content_item in content {
        match content_item.content_mut() {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if remaining == 0 {
                    continue;
                }

                let token_count = approx_token_count(text);
                if token_count <= remaining {
                    remaining = remaining.saturating_sub(token_count);
                } else {
                    *text = truncate_text(text, TruncationPolicy::Tokens(remaining));
                    remaining = 0;
                }
                if !text.is_empty() {
                    truncated_content.push(content_item);
                }
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => {
                truncated_content.push(content_item);
            }
        }
    }

    if truncated_content.is_empty() {
        return None;
    }

    set_annotated_content(&mut envelope.item, truncated_content)?;
    if let Some(metadata) = &mut envelope.metadata {
        metadata.mark_retained_sources_incomplete();
    }
    Some(envelope)
}

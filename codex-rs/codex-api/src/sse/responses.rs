use super::responses_error::parse_failed_response;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::SafetyBuffering;
use crate::common::SafetyBufferingTreatment;
use crate::error::ApiError;
use crate::error::parse_flex_unavailable;
use crate::rate_limits::parse_all_rate_limits;
use crate::safety_buffering::treatment_from_headers;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::ResponseUsageMetadata;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

const X_REASONING_INCLUDED_HEADER: &str = "x-reasoning-included";
const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const OPENAI_MODEL_HEADER: &str = "openai-model";
const REQUEST_ID_HEADER: &str = "x-request-id";
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";

pub fn spawn_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> ResponseStream {
    let rate_limit_snapshots = parse_all_rate_limits(&stream_response.headers);
    let models_etag = stream_response
        .headers
        .get("X-Models-Etag")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let server_model = stream_response
        .headers
        .get(OPENAI_MODEL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string);
    let reasoning_included = stream_response
        .headers
        .get(X_REASONING_INCLUDED_HEADER)
        .is_some();
    let upstream_request_id = stream_response
        .headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let safety_buffering_treatment =
        treatment_from_headers(&stream_response.headers).unwrap_or_default();
    if let Some(turn_state) = turn_state.as_ref()
        && let Some(header_value) = stream_response
            .headers
            .get(X_CODEX_TURN_STATE_HEADER)
            .and_then(|value| value.to_str().ok())
    {
        let _ = turn_state.set(header_value.to_string());
    }
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        if let Some(model) = server_model {
            let _ = tx_event.send(Ok(ResponseEvent::ServerModel(model))).await;
        }
        for snapshot in rate_limit_snapshots {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        if let Some(etag) = models_etag {
            let _ = tx_event.send(Ok(ResponseEvent::ModelsEtag(etag))).await;
        }
        if reasoning_included {
            let _ = tx_event
                .send(Ok(ResponseEvent::ServerReasoningIncluded(true)))
                .await;
        }
        process_sse_with_treatment(
            stream_response.bytes,
            tx_event,
            idle_timeout,
            safety_buffering_treatment,
        )
        .await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id,
        interrupt: None,
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ResponseCompleted {
    id: String,
    #[serde(default)]
    usage: Option<ResponseCompletedUsage>,
    usage_metadata: Option<ResponseUsageMetadata>,
    #[serde(default)]
    end_turn: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedUsage {
    input_tokens: i64,
    input_tokens_details: Option<ResponseCompletedInputTokensDetails>,
    output_tokens: i64,
    output_tokens_details: Option<ResponseCompletedOutputTokensDetails>,
    total_tokens: i64,
    #[serde(default)]
    codex_rollout_budget_units: Option<serde_json::Number>,
}

impl From<ResponseCompletedUsage> for TokenUsage {
    fn from(val: ResponseCompletedUsage) -> Self {
        let input_tokens_details = val.input_tokens_details.unwrap_or_default();
        TokenUsage {
            input_tokens: val.input_tokens,
            cached_input_tokens: input_tokens_details.cached_tokens,
            cache_write_input_tokens: input_tokens_details.cache_write_tokens,
            output_tokens: val.output_tokens,
            reasoning_output_tokens: val
                .output_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0),
            total_tokens: val.total_tokens,
            codex_rollout_budget_units: val.codex_rollout_budget_units,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ResponseCompletedInputTokensDetails {
    cached_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct ResponseCompletedOutputTokensDetails {
    reasoning_tokens: i64,
}

#[derive(Deserialize, Debug)]
pub struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) headers: Option<Value>,
    metadata: Option<Value>,
    response: Option<Value>,
    error: Option<Value>,
    item: Option<Value>,
    item_id: Option<String>,
    call_id: Option<String>,
    delta: Option<String>,
    text: Option<String>,
    summary_index: Option<i64>,
    content_index: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_present_value")]
    safety_buffering: Option<Value>,
}

fn deserialize_present_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

impl ResponsesStreamEvent {
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Returns the effective model reported by the server, if present.
    ///
    /// Precedence:
    /// 1. `response.headers` for standard Responses stream events.
    /// 2. top-level `headers` for websocket metadata events.
    pub fn response_model(&self) -> Option<String> {
        let response_headers_model = self
            .response
            .as_ref()
            .and_then(|response| response.get("headers"))
            .and_then(header_openai_model_value_from_json);

        match response_headers_model {
            Some(model) => Some(model),
            None => self
                .headers
                .as_ref()
                .and_then(header_openai_model_value_from_json),
        }
    }

    pub(crate) fn turn_state(&self) -> Option<String> {
        if self.kind() != "response.metadata" {
            return None;
        }

        self.headers
            .as_ref()
            .and_then(header_turn_state_value_from_json)
    }

    pub(crate) fn model_verifications(&self) -> Option<Vec<ModelVerification>> {
        if self.kind() != "response.metadata" {
            return None;
        }

        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("openai_verification_recommendation"))
            .and_then(model_verifications_from_json_value)
    }

    pub(crate) fn turn_moderation_metadata(&self) -> Option<TurnModerationMetadataEvent> {
        if self.kind() != "response.metadata" {
            return None;
        }

        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("openai_chatgpt_moderation_metadata"))
            .cloned()
            .map(|metadata| TurnModerationMetadataEvent { metadata })
    }

    pub(crate) fn safety_buffering(
        &self,
        treatment: &SafetyBufferingTreatment,
    ) -> Option<SafetyBuffering> {
        let value = self.safety_buffering.as_ref().or_else(|| {
            if self.kind() != "response.metadata" {
                return None;
            }

            let metadata = self.metadata.as_ref()?;
            if metadata.get("type").and_then(Value::as_str) != Some("safety_buffering") {
                return None;
            }
            Some(metadata)
        })?;
        let retry_model_present = value.as_object()?.contains_key("retry_model");
        let mut buffering: SafetyBuffering = serde_json::from_value(value.clone()).ok()?;
        buffering.show_buffering_ui = true;
        if !retry_model_present {
            buffering.faster_model.clone_from(&treatment.faster_model);
        }
        Some(buffering)
    }
}

fn header_openai_model_value_from_json(value: &Value) -> Option<String> {
    let headers = value.as_object()?;
    headers.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case("openai-model") || name.eq_ignore_ascii_case("x-openai-model")
        {
            json_value_as_string(value)
        } else {
            None
        }
    })
}

fn header_turn_state_value_from_json(value: &Value) -> Option<String> {
    let headers = value.as_object()?;
    headers.iter().find_map(|(name, value)| {
        if name.eq_ignore_ascii_case(X_CODEX_TURN_STATE_HEADER) {
            json_value_as_string(value)
        } else {
            None
        }
    })
}

fn model_verifications_from_json_value(value: &Value) -> Option<Vec<ModelVerification>> {
    let verifications = value
        .as_array()
        .map(|items| {
            let mut verifications = Vec::new();
            for verification in items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(parse_model_verification)
            {
                if !verifications.contains(&verification) {
                    verifications.push(verification);
                }
            }
            verifications
        })
        .unwrap_or_default();

    if verifications.is_empty() {
        None
    } else {
        Some(verifications)
    }
}

fn parse_model_verification(value: &str) -> Option<ModelVerification> {
    match value {
        TRUSTED_ACCESS_FOR_CYBER_VERIFICATION => Some(ModelVerification::TrustedAccessForCyber),
        _ => None,
    }
}

fn json_value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(items) => items.first().and_then(json_value_as_string),
        _ => None,
    }
}

#[derive(Debug)]
pub enum ResponsesEventError {
    Api(ApiError),
}

impl ResponsesEventError {
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::Api(error) => error,
        }
    }
}

pub fn process_responses_event(
    event: ResponsesStreamEvent,
) -> std::result::Result<Option<ResponseEvent>, ResponsesEventError> {
    match event.kind.as_str() {
        "error" => {
            if let Some(error) = event.error.as_ref().and_then(parse_flex_unavailable) {
                return Err(ResponsesEventError::Api(error));
            }
        }
        "response.output_item.done" => {
            if let Some(item_val) = event.item {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(item_val) {
                    return Ok(Some(ResponseEvent::OutputItemDone(item)));
                }
                debug!("failed to parse ResponseItem from output_item.done");
            }
        }
        "response.output_text.delta" => {
            if let Some(delta) = event.delta {
                return Ok(Some(ResponseEvent::OutputTextDelta(delta)));
            }
        }
        "response.custom_tool_call_input.delta" => {
            if let (Some(delta), Some(item_id)) =
                (event.delta, event.item_id.clone().or(event.call_id.clone()))
            {
                return Ok(Some(ResponseEvent::ToolCallInputDelta {
                    item_id,
                    call_id: event.call_id,
                    delta,
                }));
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let (Some(delta), Some(summary_index)) = (event.delta, event.summary_index) {
                return Ok(Some(ResponseEvent::ReasoningSummaryDelta {
                    delta,
                    summary_index,
                }));
            }
        }
        "response.reasoning_summary_text.done" => {
            if let (Some(item_id), Some(text), Some(summary_index)) =
                (event.item_id, event.text, event.summary_index)
            {
                return Ok(Some(ResponseEvent::ReasoningSummaryDone {
                    item_id,
                    text,
                    summary_index,
                }));
            }
        }
        "response.reasoning_text.delta" => {
            if let (Some(delta), Some(content_index)) = (event.delta, event.content_index) {
                return Ok(Some(ResponseEvent::ReasoningContentDelta {
                    delta,
                    content_index,
                }));
            }
        }
        "response.created" => {
            if let Some(response) = event.response {
                let response_id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                return Ok(Some(ResponseEvent::Created { response_id }));
            }
        }
        "response.failed" => {
            return Err(ResponsesEventError::Api(parse_failed_response(
                event.response,
            )));
        }
        "response.completed" | "response.incomplete" => {
            let interrupted = event.kind == "response.incomplete";
            if interrupted {
                let reason = event.response.as_ref().and_then(|response| {
                    response
                        .get("incomplete_details")
                        .and_then(|details| details.get("reason"))
                        .and_then(Value::as_str)
                });
                let reason = reason.unwrap_or("unknown");
                if reason == "content_filter" {
                    return Err(ResponsesEventError::Api(ApiError::ContentFilter));
                }
                if reason != "interrupted" {
                    return Err(ResponsesEventError::Api(ApiError::ResponseIncomplete(
                        reason.to_string(),
                    )));
                }
            }
            if let Some(resp_val) = event.response {
                let metadata = resp_val
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .cloned();
                match serde_json::from_value::<ResponseCompleted>(resp_val) {
                    Ok(mut resp) => {
                        if let Some(metadata) = metadata {
                            resp.usage_metadata.get_or_insert_default().metadata = Some(metadata);
                        }
                        return Ok(Some(ResponseEvent::Completed {
                            response_id: resp.id,
                            token_usage: resp.usage.map(Into::into),
                            usage_metadata: resp.usage_metadata,
                            end_turn: if interrupted {
                                Some(false)
                            } else {
                                resp.end_turn
                            },
                        }));
                    }
                    Err(err) => {
                        let error = format!("failed to parse ResponseCompleted: {err}");
                        debug!("{error}");
                        return Err(ResponsesEventError::Api(ApiError::Stream(error)));
                    }
                }
            }
        }
        "response.output_item.added" => {
            if let Some(item_val) = event.item {
                if let Ok(item) = serde_json::from_value::<ResponseItem>(item_val) {
                    return Ok(Some(ResponseEvent::OutputItemAdded(item)));
                }
                debug!("failed to parse ResponseItem from output_item.added");
            }
        }
        "response.reasoning_summary_part.added" => {
            if let Some(summary_index) = event.summary_index {
                return Ok(Some(ResponseEvent::ReasoningSummaryPartAdded {
                    summary_index,
                }));
            }
        }
        "codex.response.metadata"
        | "response.content_part.added"
        | "response.content_part.done"
        | "response.custom_tool_call_input.done"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done"
        | "response.in_progress"
        | "response.metadata"
        | "response.output_text.done"
        | "response.reasoning_summary_part.done"
        | "responsesapi.websocket_timing" => {
            trace!("unhandled responses event: {}", event.kind);
        }
        kind if kind.ends_with(".delta") => {
            trace!("unhandled responses event: {kind}");
        }
        _ => {
            debug!(
                "unhandled responses event: {:?}",
                event.kind.chars().take(128).collect::<String>()
            );
        }
    }

    Ok(None)
}

async fn process_sse_with_treatment(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    safety_buffering_treatment: SafetyBufferingTreatment,
) {
    let mut stream = stream.eventsource();
    let mut response_error: Option<ApiError> = None;
    let mut last_server_model: Option<String> = None;

    loop {
        let response = tokio::select! {
            biased;
            _ = tx_event.closed() => return,
            response = timeout(idle_timeout, stream.next()) => response,
        };
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                return;
            }
            Ok(None) => {
                let error = response_error.unwrap_or(ApiError::Stream(
                    "stream closed before response.completed".into(),
                ));
                let _ = tx_event.send(Err(error)).await;
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                return;
            }
        };

        trace!("SSE event: {}", &sse.data);

        let event: ResponsesStreamEvent = match serde_json::from_str(&sse.data) {
            Ok(event) => event,
            Err(e) => {
                debug!(
                    error_category = ?e.classify(),
                    error_line = e.line(),
                    error_column = e.column(),
                    payload_bytes = sse.data.len(),
                    "Failed to parse SSE event"
                );
                continue;
            }
        };
        let model_verifications = event.model_verifications();
        let turn_moderation_metadata = event.turn_moderation_metadata();
        let safety_buffering = event.safety_buffering(&safety_buffering_treatment);

        if let Some(model) = event.response_model()
            && last_server_model.as_deref() != Some(model.as_str())
        {
            if tx_event
                .send(Ok(ResponseEvent::ServerModel(model.clone())))
                .await
                .is_err()
            {
                return;
            }
            last_server_model = Some(model);
        }
        if let Some(verifications) = model_verifications
            && tx_event
                .send(Ok(ResponseEvent::ModelVerifications(verifications)))
                .await
                .is_err()
        {
            return;
        }
        if let Some(metadata) = turn_moderation_metadata
            && tx_event
                .send(Ok(ResponseEvent::TurnModerationMetadata(metadata)))
                .await
                .is_err()
        {
            return;
        }
        if let Some(buffering) = safety_buffering
            && tx_event
                .send(Ok(ResponseEvent::SafetyBuffering(buffering)))
                .await
                .is_err()
        {
            return;
        }

        match process_responses_event(event) {
            Ok(Some(event)) => {
                let is_completed = matches!(event, ResponseEvent::Completed { .. });
                if tx_event.send(Ok(event)).await.is_err() {
                    return;
                }
                if is_completed {
                    return;
                }
            }
            Ok(None) => {}
            Err(error) => {
                let error = error.into_api_error();
                if matches!(
                    error,
                    ApiError::FlexUnavailable | ApiError::ResponseIncomplete(_)
                ) {
                    let _ = tx_event.send(Err(error)).await;
                    return;
                }
                response_error = Some(error);
            }
        };
    }
}

//! Records attempted calls at existing execution boundaries. Request metadata policy
//! lives in the private request metadata module; this recorder must never dispatch or await tools.

#[cfg(test)]
#[path = "executed_tool_calls_direct_tests.rs"]
mod direct_tests;
mod mcp_attribution;
mod request_metadata;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_features::Feature;
use codex_features::Features;
use codex_history::InitialHistory;
use codex_protocol::ResponseItemId;
use codex_protocol::mcp::McpAttribution;
use codex_protocol::mcp::McpAttributionSource;
use codex_protocol::models::ExecutedToolCall;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::executed_tool_call_metadata_bytes;
use serde_json::Value as JsonValue;

use crate::session::session::Session;
use crate::tools::context::ToolPayload;
use crate::tools::metadata_metrics;
use crate::tools::router::ToolCall;
use codex_protocol::ToolName;

const MAX_PENDING_EXECUTED_TOOL_CALLS: usize = 256;
// Limits new Direct metadata retained in history and the append-only rollout
// during one recorder lifetime. A new process cannot recover the exact prior
// budget because raw result metadata is intentionally not deserialized.
const MAX_RETAINED_DIRECT_METADATA_BYTES: usize = 1024 * 1024;

/// Best-effort recording shared by a session's tool calls. Disabled sessions
/// allocate no recorder state; missing evidence remains incomplete.
#[derive(Clone, Default)]
pub(crate) struct ExecutedToolCalls {
    state: Arc<Mutex<Option<ExecutedToolCallRecorderState>>>,
    retained_direct_metadata_bytes: Arc<AtomicUsize>,
    pending_direct_calls: Arc<AtomicUsize>,
    mcp_attribution: mcp_attribution::McpAttributionRecorder,
}

// Avoid exposing recorded call arguments or result metadata through client Debug output.
impl std::fmt::Debug for ExecutedToolCalls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecutedToolCalls")
            .finish_non_exhaustive()
    }
}

// The tool future owns this reservation, so completion or cancellation releases it.
pub(crate) struct DirectCallPermit {
    recording: Weak<()>,
    pending_direct_calls: Arc<AtomicUsize>,
}

impl DirectCallPermit {
    pub(crate) fn strong_count(&self) -> usize {
        self.recording.strong_count()
    }
}

impl Drop for DirectCallPermit {
    fn drop(&mut self) {
        self.pending_direct_calls.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct ExecutedToolCallRecorderState {
    // Disabling drops this lifetime, permanently invalidating prepared Direct records.
    recording: Arc<()>,
    // Full live Direct observations; persisted outputs keep their bounded projection.
    direct_calls: HashMap<ResponseItemId, ExecutedToolCall>,
}

impl ExecutedToolCalls {
    pub(crate) fn new(features: &Features, history: &InitialHistory) -> Self {
        Self {
            state: Arc::new(Mutex::new(
                Self::is_enabled(features).then(ExecutedToolCallRecorderState::default),
            )),
            retained_direct_metadata_bytes: Arc::new(AtomicUsize::new(0)),
            pending_direct_calls: Arc::new(AtomicUsize::new(0)),
            mcp_attribution: mcp_attribution::McpAttributionRecorder::new(history),
        }
    }

    pub(crate) fn record_mcp_source(&self, source: McpAttributionSource) {
        self.mcp_attribution.record(source);
    }

    pub(crate) fn mcp_attribution_snapshot(&self) -> McpAttribution {
        self.mcp_attribution.snapshot()
    }

    pub(crate) fn mcp_attribution_checkpoint(&self, force: bool) -> Option<(McpAttribution, u64)> {
        self.mcp_attribution.checkpoint(force)
    }

    pub(crate) fn mark_mcp_attribution_persisted(&self, revision: u64) {
        self.mcp_attribution.mark_persisted(revision);
    }

    fn lock_state(&self) -> MutexGuard<'_, Option<ExecutedToolCallRecorderState>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Called under the session config lock; this never changes execution features.
    pub(crate) fn refresh(&self, features: &Features) {
        let enabled = Self::is_enabled(features);
        let mut state = self.lock_state();
        if enabled != state.is_some() {
            *state = enabled.then(ExecutedToolCallRecorderState::default);
        }
    }

    /// The turn's feature policy is independent of whether this session has a recorder.
    pub(crate) fn is_enabled(features: &Features) -> bool {
        features.enabled(Feature::ExecutedToolCallMetadata)
    }

    /// Keep a Direct record with its invocation so it can be attached to that invocation's output.
    pub(crate) fn prepare_direct_call(
        &self,
        call: &ToolCall,
    ) -> Option<(ExecutedToolCall, DirectCallPermit)> {
        let permit = self.reserve_direct_call()?;
        Some((recorded_call(call), permit))
    }

    fn reserve_direct_call(&self) -> Option<DirectCallPermit> {
        let recording = Arc::downgrade(&self.lock_state().as_ref()?.recording);
        self.pending_direct_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |pending| {
                (pending < MAX_PENDING_EXECUTED_TOOL_CALLS).then_some(pending + 1)
            })
            .ok()?;
        Some(DirectCallPermit {
            recording,
            pending_direct_calls: Arc::clone(&self.pending_direct_calls),
        })
    }

    /// Validate the recording lifetime and attach together so a config refresh
    /// cannot invalidate the call between those two operations.
    pub(crate) fn attach_direct_call_to_output(
        &self,
        item: &mut ResponseItem,
        prepared: Option<(ExecutedToolCall, DirectCallPermit)>,
    ) {
        let Some((call, permit)) = prepared else {
            return;
        };
        let mut state = self.lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        if !permit.recording.ptr_eq(&Arc::downgrade(&state.recording)) {
            return;
        }
        // Assign the output's ordinary history ID here so reused model call IDs
        // cannot associate this observation with a different output.
        Session::assign_missing_response_item_id(item);
        if let Some(id) = item.id() {
            state.direct_calls.insert(id.clone(), call.clone());
        }
        let complete = call.has_complete_inventory();
        item.append_executed_tool_calls(vec![call]);
        if complete {
            item.mark_tool_calls_complete();
        }
        let retained = self.retained_direct_metadata_bytes.load(Ordering::Relaxed);
        let available = MAX_RETAINED_DIRECT_METADATA_BYTES.saturating_sub(retained);
        let mut bytes = executed_tool_call_metadata_bytes(item);
        let original_bytes = bytes;
        if bytes > available {
            item.retain_tool_resource_access_or_omit_metadata(bytes - available);
            bytes = executed_tool_call_metadata_bytes(item);
        }
        if bytes > available {
            item.omit_tool_result_metadata(bytes - available);
            bytes = executed_tool_call_metadata_bytes(item);
        }
        if bytes > available {
            item.clear_tool_result_metadata();
            bytes = executed_tool_call_metadata_bytes(item);
        }
        if bytes > available {
            item.clear_executed_tool_calls();
            metadata_metrics::record_shedding(
                "direct_retained",
                original_bytes,
                executed_tool_call_metadata_bytes(item),
                codex_otel::global().as_ref(),
            );
            return;
        }
        // All Direct attachments share the recorder lock, including cloned handles.
        self.retained_direct_metadata_bytes
            .store(retained + bytes, Ordering::Relaxed);
        metadata_metrics::record_shedding(
            "direct_retained",
            original_bytes,
            bytes,
            codex_otel::global().as_ref(),
        );
    }
}

fn recorded_call(call: &ToolCall) -> ExecutedToolCall {
    let name = flat_tool_name(&call.tool_name);
    let arguments = match &call.payload {
        ToolPayload::Function { arguments } => {
            serde_json::from_str(arguments).unwrap_or_else(|_| JsonValue::String(arguments.clone()))
        }
        ToolPayload::Custom { input } => JsonValue::String(input.clone()),
        ToolPayload::ToolSearch { arguments } => {
            serde_json::to_value(arguments).unwrap_or_default()
        }
    };
    ExecutedToolCall::new(name, arguments)
}

/// Server-facing flat name: namespaced tools join with `__` unless a separator is already present.
fn flat_tool_name(tool_name: &ToolName) -> String {
    if tool_name.is_default_namespace() {
        return tool_name.name.clone();
    }
    match tool_name.namespace.as_deref() {
        Some(namespace) if namespace.ends_with('_') || tool_name.name.starts_with('_') => {
            format!("{namespace}{}", tool_name.name)
        }
        Some(namespace) => format!("{namespace}__{}", tool_name.name),
        None => tool_name.name.clone(),
    }
}

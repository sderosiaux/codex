//! Attaches host observations to request metadata without changing tool execution.

use std::collections::HashSet;

use codex_protocol::models::ResponseItem;
use codex_protocol::models::normalize_executed_tool_call_completeness;

use super::ExecutedToolCalls;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Sampling,
    Compaction,
}

impl ExecutedToolCalls {
    pub(crate) fn attach_to_compaction_prompt(&self, items: &mut [ResponseItem]) {
        self.attach_to_request(items, RequestKind::Compaction);
    }

    pub(crate) fn attach_to_prompt(&self, items: &mut [ResponseItem]) {
        self.attach_to_request(items, RequestKind::Sampling);
    }

    fn attach_to_request(&self, items: &mut [ResponseItem], request_kind: RequestKind) {
        {
            let mut state = self.lock_state();
            let Some(state) = state.as_mut() else {
                // Disabling capture also stops replaying Direct records from history.
                clear_direct_call_metadata(items);
                return;
            };
            for item in items.iter_mut() {
                if let Some(call) = item.id().and_then(|id| state.direct_calls.get(id)) {
                    item.clear_executed_tool_calls();
                    item.append_executed_tool_calls(vec![call.clone()]);
                    if call.has_complete_inventory() {
                        item.mark_tool_calls_complete();
                    }
                }
            }
            // A shortened compaction attempt is not an installed history window.
            // Only ordinary sampling can discard observations absent from the window.
            if request_kind == RequestKind::Sampling && !state.direct_calls.is_empty() {
                let output_ids = items
                    .iter()
                    .filter_map(ResponseItem::id)
                    .collect::<HashSet<_>>();
                state.direct_calls.retain(|id, _| output_ids.contains(id));
            }
        }

        // Argument truncation preserves inventory completeness; omitted calls or
        // names still invalidate completion.
        normalize_executed_tool_call_completeness(items);
    }
}

fn clear_direct_call_metadata(items: &mut [ResponseItem]) {
    for item in items
        .iter_mut()
        .filter(|item| has_direct_call_metadata(item))
    {
        item.clear_executed_tool_calls();
    }
}

fn has_direct_call_metadata(item: &ResponseItem) -> bool {
    item.executed_tool_call_metadata().is_some_and(|metadata| {
        metadata.cell_id.is_none() && metadata.executed_tool_calls.is_some()
    })
}

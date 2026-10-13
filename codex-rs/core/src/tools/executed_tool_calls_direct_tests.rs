//! Tests Direct metadata admission and permit lifetimes.
//! Metadata limits must leave ordinary tool outputs unchanged.

use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ToolResultMetadata;

use crate::utils::json::serialized_json_bytes;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn new_recorder(history: InitialHistory) -> ExecutedToolCalls {
    let mut features = Features::default();
    features.enable(Feature::ExecutedToolCallMetadata);
    ExecutedToolCalls::new(&features, &history)
}

fn output(call_id: &str) -> ResponseItem {
    ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text("tool result".to_string()),
    })
}

#[test]
fn direct_request_uses_live_window_after_persisted_budget_is_exhausted() {
    let mut features = Features::default();
    features.enable(Feature::ExecutedToolCallMetadata);
    let recorder = ExecutedToolCalls::new(&features, &InitialHistory::New);
    recorder
        .retained_direct_metadata_bytes
        .store(MAX_RETAINED_DIRECT_METADATA_BYTES, Ordering::Relaxed);
    let metadata = json!({"openai/resource_access": {"connector": "example"}});
    let make_output = |id: &str| {
        let mut call = ExecutedToolCall::new("connector_tool".to_string(), json!({}));
        call.set_tool_result_metadata(ToolResultMetadata::new(&metadata));
        let mut item = output(id);
        recorder.attach_direct_call_to_output(
            &mut item,
            Some((call, recorder.reserve_direct_call().unwrap())),
        );
        item
    };
    let old = make_output("old-window");
    let mut first = vec![old.clone()];
    recorder.attach_to_prompt(&mut first);
    let current = make_output("new-window");
    assert!(current.executed_tool_call_metadata().is_none());
    let serialized = serde_json::to_string(&codex_history::RolloutItem::ResponseItem(
        current.clone().into(),
    ))
    .unwrap();
    assert!(!serialized.contains("example"));

    // The input to the next window no longer contains the old output.
    let mut next = vec![current.clone()];
    recorder.attach_to_prompt(&mut next);
    for item in [&first[0], &next[0]] {
        let encoded = serde_json::to_value(item).unwrap();
        assert_eq!(encoded["output"], "tool result");
        assert_eq!(
            encoded["internal_chat_message_metadata_passthrough"]["executed_tool_calls"][0]["tool_result_metadata"],
            metadata,
        );
    }
    let mut pruned = vec![old];
    recorder.attach_to_compaction_prompt(&mut pruned);
    assert!(pruned[0].executed_tool_call_metadata().is_none());

    // Capture toggles must not revive a previous recorder's live-only metadata.
    recorder.refresh(&Features::default());
    recorder.refresh(&features);
    let mut after_refresh = vec![current];
    recorder.attach_to_prompt(&mut after_refresh);
    assert!(after_refresh[0].executed_tool_call_metadata().is_none());
}

#[test]
fn direct_metadata_follows_output_ids_with_reused_call_ids_and_reverse_completion() {
    for supplied_ids in [false, true] {
        let mut features = Features::default();
        features.enable(Feature::ExecutedToolCallMetadata);
        let recorder = ExecutedToolCalls::new(&features, &InitialHistory::New);
        recorder
            .retained_direct_metadata_bytes
            .store(MAX_RETAINED_DIRECT_METADATA_BYTES, Ordering::Relaxed);
        let metadata = [json!({"result": "first"}), json!({"result": "second"})];
        let mut prepared = metadata
            .iter()
            .map(|metadata| {
                let mut call = ExecutedToolCall::new("connector_tool".to_string(), json!({}));
                call.set_tool_result_metadata(ToolResultMetadata::new(metadata));
                Some((call, recorder.reserve_direct_call().unwrap()))
            })
            .collect::<Vec<_>>();
        let mut outputs = [output("reused"), output("reused")];
        let expected_ids = [
            ResponseItemId::with_suffix("fco", "first"),
            ResponseItemId::with_suffix("fco", "second"),
        ];
        if supplied_ids {
            for (item, id) in outputs.iter_mut().zip(&expected_ids) {
                item.set_id(Some(id.clone()));
            }
        }
        // Completion order and prompt order must not choose which observation belongs here.
        for index in [1, 0] {
            recorder.attach_direct_call_to_output(&mut outputs[index], prepared[index].take());
            assert!(outputs[index].executed_tool_call_metadata().is_none());
            if supplied_ids {
                assert_eq!(outputs[index].id(), Some(&expected_ids[index]));
            }
        }
        let ids = outputs.clone().map(|item| item.id().unwrap().clone());
        assert_ne!(ids[0], ids[1]);
        let mut request = outputs;
        for reversed in [false, true] {
            if reversed {
                request.reverse();
            }
            recorder.attach_to_prompt(&mut request);
            for item in &request {
                let index = usize::from(item.id() == Some(&ids[1]));
                let encoded = serde_json::to_value(item).unwrap();
                assert_eq!(encoded["call_id"], "reused");
                assert_eq!(encoded["output"], "tool result");
                assert_eq!(
                    encoded["internal_chat_message_metadata_passthrough"]["executed_tool_calls"][0]
                        ["tool_result_metadata"],
                    metadata[index],
                );
            }
        }
    }
}

#[test]
fn direct_budget_keeps_an_omission_marker_when_it_fits() {
    for metadata in [
        json!({"provider": "x".repeat(1024)}),
        json!({"openai/resource_access": "x".repeat(1024)}),
    ] {
        let mut features = Features::default();
        features.enable(Feature::ExecutedToolCallMetadata);
        let recorder = ExecutedToolCalls::new(&features, &InitialHistory::New);
        let mut call = ExecutedToolCall::new("test_tool".to_string(), json!({"argument": "kept"}));
        call.set_tool_result_metadata(ToolResultMetadata::new(&metadata));
        let mut expected = output("direct");
        expected.append_executed_tool_calls(vec![call.clone()]);
        expected.mark_tool_calls_complete();
        let available = 512;
        let overage = executed_tool_call_metadata_bytes(&expected) - available;
        let mut omitted =
            ExecutedToolCall::new("test_tool".to_string(), json!({"argument": "kept"}));
        omitted.set_tool_result_metadata(ToolResultMetadata::new(&json!(format!(
            "omitted_due_to_size_limit (overage_bytes={overage})"
        ))));
        expected.clear_executed_tool_calls();
        expected.append_executed_tool_calls(vec![omitted]);
        expected.mark_tool_calls_complete();
        let retained = MAX_RETAINED_DIRECT_METADATA_BYTES - available;
        recorder
            .retained_direct_metadata_bytes
            .store(retained, Ordering::Relaxed);

        let mut item = output("direct");
        recorder.attach_direct_call_to_output(
            &mut item,
            Some((call, recorder.reserve_direct_call().unwrap())),
        );

        expected.set_id(item.id().cloned());
        assert_eq!(item, expected);
        assert_eq!(
            recorder
                .retained_direct_metadata_bytes
                .load(Ordering::Relaxed),
            retained + executed_tool_call_metadata_bytes(&expected),
        );
    }
}

#[test]
fn direct_pending_limit_releases_on_completion_or_dropped_future() {
    let mut features = Features::default();
    features.enable(Feature::ExecutedToolCallMetadata);
    let recorder = ExecutedToolCalls::new(&features, &InitialHistory::New);
    let call = ToolCall {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        call_id: "direct".to_string(),
        payload: ToolPayload::Function {
            arguments: json!({ "argument": "kept" }).to_string(),
        },
        encrypted_function_args: None,
    };
    let prepare = |recorder: &ExecutedToolCalls| recorder.prepare_direct_call(&call);
    let mut pending = (0..MAX_PENDING_EXECUTED_TOOL_CALLS)
        .map(|_| prepare(&recorder).expect("metadata slot"))
        .collect::<Vec<_>>();
    let cloned = recorder.clone();
    let mut overflow = output("overflow");
    let before = serde_json::to_value(&overflow).expect("serializable output");
    cloned.attach_direct_call_to_output(&mut overflow, prepare(&cloned));
    assert_eq!(
        serde_json::to_value(&overflow).expect("serializable output"),
        before,
    );

    let canceled = pending.pop().expect("pending call");
    let unpolled = async move {
        std::future::pending::<()>().await;
        drop(canceled);
    };
    assert!(prepare(&cloned).is_none());
    drop(unpolled);
    let replacement = prepare(&cloned).expect("dropped future released its slot");
    assert!(prepare(&recorder).is_none());

    let mut completed = output("completed");
    recorder.attach_direct_call_to_output(&mut completed, pending.pop());
    assert_eq!(
        completed
            .executed_tool_call_metadata()
            .and_then(|metadata| metadata.tool_calls_complete),
        Some(true),
    );
    let after_completion = prepare(&recorder).expect("completion released its slot");

    recorder.refresh(&Features::default());
    recorder.refresh(&features);
    assert!(prepare(&recorder).is_none());
    let mut stale = output("stale");
    recorder.attach_direct_call_to_output(&mut stale, Some(replacement));
    assert!(stale.executed_tool_call_metadata().is_none());
    let current = prepare(&recorder).expect("stale call released its slot");
    let mut current_output = output("current");
    recorder.attach_direct_call_to_output(&mut current_output, Some(current));
    assert_eq!(
        current_output
            .executed_tool_call_metadata()
            .and_then(|metadata| metadata.tool_calls_complete),
        Some(true),
    );
    drop(pending);
    drop(after_completion);
    assert_eq!(recorder.pending_direct_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn direct_recording_preserves_encoded_arguments_in_history() {
    let mut features = Features::default();
    features.enable(Feature::ExecutedToolCallMetadata);
    let recorder = ExecutedToolCalls::new(&features, &InitialHistory::New);
    let invalid_json = "\"".repeat(5_000);
    let wire_bytes = serialized_json_bytes(&JsonValue::String(invalid_json.clone())).unwrap();
    assert!(wire_bytes > 8 * 1024);
    let call = ToolCall {
        tool_name: codex_tools::ToolName::plain("test_tool"),
        call_id: "direct".to_string(),
        payload: ToolPayload::Function {
            arguments: invalid_json.clone(),
        },
        encrypted_function_args: None,
    };
    let mut item = output("direct");
    let ordinary_output = serde_json::to_value(&item).unwrap()["output"].clone();
    recorder.attach_direct_call_to_output(&mut item, recorder.prepare_direct_call(&call));
    let wire = serde_json::to_value(&item).unwrap();
    assert_eq!(wire["output"], ordinary_output);
    let metadata = &wire["internal_chat_message_metadata_passthrough"];
    let arguments = &metadata["executed_tool_calls"][0]["arguments"];
    assert_eq!(arguments, &JsonValue::String(invalid_json));
    assert_eq!(serialized_json_bytes(arguments).unwrap(), wire_bytes);
    assert_eq!(metadata["tool_calls_complete"], true);
}

#[test]
fn direct_retained_metadata_budget_sheds_results_then_calls_across_refresh() {
    let recorder = new_recorder(InitialHistory::New);
    let resource_access = json!({"resource_coverage": "complete", "resources": []});
    let mut call = ExecutedToolCall::new("test_tool".to_string(), json!({"argument": "kept"}));
    call.set_tool_result_metadata(ToolResultMetadata::new(&json!({
        "provider": "x".repeat(1024),
        "openai/resource_access": resource_access,
    })));
    let mut first = output("first");
    recorder.attach_direct_call_to_output(
        &mut first,
        Some((call.clone(), recorder.reserve_direct_call().unwrap())),
    );
    let mut expected_first = output("first");
    expected_first.set_id(first.id().cloned());
    expected_first.append_executed_tool_calls(vec![call.clone()]);
    expected_first.mark_tool_calls_complete();
    assert_eq!(first, expected_first);
    let full_bytes = executed_tool_call_metadata_bytes(&first);
    assert!(full_bytes > 0);
    assert_eq!(
        recorder
            .retained_direct_metadata_bytes
            .load(Ordering::Relaxed),
        full_bytes,
    );

    let mut resource_only_call = call.clone();
    resource_only_call.set_tool_result_metadata(ToolResultMetadata::new(&json!({
        "openai/resource_access": resource_access,
    })));
    let mut resource_only_output = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: "resource-only".to_string(),
        output: FunctionCallOutputPayload::from_text("ordinary result".to_string()),
    });
    let mut expected_resource_only = resource_only_output.clone();
    expected_resource_only.append_executed_tool_calls(vec![resource_only_call]);
    expected_resource_only.mark_tool_calls_complete();
    let resource_only_bytes = executed_tool_call_metadata_bytes(&expected_resource_only);
    assert!(full_bytes > resource_only_bytes);
    recorder.retained_direct_metadata_bytes.store(
        MAX_RETAINED_DIRECT_METADATA_BYTES - resource_only_bytes,
        Ordering::Relaxed,
    );
    recorder.clone().attach_direct_call_to_output(
        &mut resource_only_output,
        Some((call.clone(), recorder.reserve_direct_call().unwrap())),
    );
    expected_resource_only.set_id(resource_only_output.id().cloned());
    assert_eq!(resource_only_output, expected_resource_only);
    assert_eq!(
        recorder
            .retained_direct_metadata_bytes
            .load(Ordering::Relaxed),
        MAX_RETAINED_DIRECT_METADATA_BYTES,
    );

    let mut without_result = first.clone();
    without_result.clear_tool_result_metadata();
    let call_bytes = executed_tool_call_metadata_bytes(&without_result);
    assert!(resource_only_bytes > call_bytes);
    recorder.retained_direct_metadata_bytes.store(
        MAX_RETAINED_DIRECT_METADATA_BYTES - call_bytes,
        Ordering::Relaxed,
    );
    let mut second = output("second");
    let result_before =
        serde_json::to_value(&second).expect("serializable output")["output"].clone();
    recorder.clone().attach_direct_call_to_output(
        &mut second,
        Some((call.clone(), recorder.reserve_direct_call().unwrap())),
    );
    let mut expected_second = output("second");
    expected_second.set_id(second.id().cloned());
    expected_second.append_executed_tool_calls(vec![ExecutedToolCall::new(
        "test_tool".to_string(),
        json!({"argument": "kept"}),
    )]);
    expected_second.mark_tool_calls_complete();
    assert_eq!(second, expected_second);
    let second_metadata = second
        .executed_tool_call_metadata()
        .expect("call remains when only its result metadata exceeds the budget");
    assert_eq!(second_metadata.tool_calls_complete, Some(true));
    assert_eq!(
        second_metadata.executed_tool_calls.as_ref().map(Vec::len),
        Some(1)
    );
    let serialized = serde_json::to_value(&second).expect("serializable output");
    assert!(
        serialized["internal_chat_message_metadata_passthrough"]["executed_tool_calls"][0]
            .get("tool_result_metadata")
            .is_none()
    );
    assert_eq!(serialized["output"], result_before);
    assert_eq!(
        recorder
            .retained_direct_metadata_bytes
            .load(Ordering::Relaxed),
        MAX_RETAINED_DIRECT_METADATA_BYTES,
    );

    let mut features = Features::default();
    recorder.refresh(&features);
    features.enable(Feature::ExecutedToolCallMetadata);
    recorder.refresh(&features);
    let mut third = output("third");
    recorder.attach_direct_call_to_output(
        &mut third,
        Some((call, recorder.reserve_direct_call().unwrap())),
    );
    assert!(third.executed_tool_call_metadata().is_none());
    assert_eq!(
        recorder
            .retained_direct_metadata_bytes
            .load(Ordering::Relaxed),
        MAX_RETAINED_DIRECT_METADATA_BYTES,
    );
}

#[tokio::test]
async fn stale_runtime_refresh_preserves_recording_state() {
    let (session, _turn) = crate::session::tests::make_session_and_context().await;
    let calls = session.services.executed_tool_calls.clone();
    assert!(calls.lock_state().is_none());
    let stale_owner = session.get_config().await;
    let mut stale_config = stale_owner.as_ref().clone();
    stale_config
        .features
        .enable(Feature::ExecutedToolCallMetadata);
    assert_eq!(
        session
            .refresh_mcp_config(Arc::clone(&stale_owner), stale_owner.as_ref().clone())
            .await,
        crate::ConfigRefreshOutcome::Published
    );

    assert_eq!(
        session
            .refresh_runtime_config(stale_owner, stale_config)
            .await,
        crate::ConfigRefreshOutcome::Stale
    );
    assert!(calls.lock_state().is_none());
}

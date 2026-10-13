//! Active settings keep environment selection and model changes scoped to the running turn.

use super::*;
use codex_exec_server::CreateDirectoryOptions;
use codex_extension_api::ExtensionDataInit;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::EnvironmentCapabilityRoots;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentRequests;
use codex_protocol::protocol::TurnEnvironmentSelection;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::environment_config_for_selection;
use pretty_assertions::assert_eq;
use test_case::test_case;

/// Active updates restore selected roots even when the request only names the environment.
#[test_case(false; "without roots")]
#[test_case(true; "with roots")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_turn_environment_selections_follow_active_updates(with_roots: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_sse_sequence(
        &server,
        vec![paused_response("first", "pause"), sse_completed("second")],
    )
    .await;
    let test = step_settings_test().build_with_auto_env(&server).await?;
    let selection = test.executor_environment().selection().clone();
    let roots = if with_roots {
        vec![SelectedCapabilityRoot {
            id: "selected-root".to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: selection.environment_id.clone(),
                path: selection.cwd.clone(),
            },
        }]
    } else {
        Vec::new()
    };
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(roots.clone());
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![selection.into_request()]),
            thread_extension_init,
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?
        .thread;
    let thread = &thread;
    let turn_id = start_paused_turn(thread).await?.turn_id;
    let initial = thread
        .active_turn_environment_selections()
        .await
        .expect("running turn");
    assert_eq!(initial.len(), 1);
    assert_eq!(
        EnvironmentCapabilityRoots::collect(
            initial
                .iter()
                .map(|selection| &selection.selected_capability_roots)
        ),
        roots,
    );
    assert_eq!(
        thread.current_turn_environment_selections(&turn_id).await,
        Some(initial.clone())
    );
    assert_eq!(
        thread
            .current_turn_environment_selections("other-turn")
            .await,
        None
    );

    for environments in [vec![], initial.clone()] {
        let requested = environments
            .clone()
            .into_iter()
            .map(TurnEnvironmentSelection::into_request)
            .collect();
        apply_turn_settings(
            thread,
            &turn_id,
            TurnSettingsUpdate {
                environments: Some(requested),
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            thread.current_turn_environment_selections(&turn_id).await,
            Some(environments)
        );
        assert_eq!(
            thread.active_turn_environment_selections().await,
            Some(initial.clone())
        );
    }

    answer_paused_turn(thread, &turn_id).await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(
        thread.current_turn_environment_selections(&turn_id).await,
        None
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_update_preserves_active_environment_and_next_turn_uses_new_selection() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let patch_response = |id, contents| {
        sse(vec![
            ev_response_created(id),
            ev_apply_patch_custom_tool_call(
                id,
                &format!("*** Begin Patch\n*** Add File: marker.txt\n+{contents}\n*** End Patch\n"),
            ),
            ev_completed(id),
        ])
    };
    let responses = mount_sse_sequence(
        &server,
        vec![
            paused_response("resp-a", "pause-a"),
            patch_response("active-patch", "active step"),
            sse_completed("active-done"),
            patch_response("next-turn-patch", "next turn"),
            sse_completed("next-turn-done"),
        ],
    )
    .await;
    let test = direct_tool_settings_test()
        .with_config(|config| {
            for model in &mut config.model_catalog.as_mut().expect("models").models {
                model.apply_patch_tool_type = Some(ApplyPatchToolType::Freeform);
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let mut next_environment = test.executor_environment().selection().clone();
    next_environment.cwd = test.workspace_path_uri("future-environment")?;
    next_environment.workspace_roots = vec![next_environment.cwd.clone()];
    test.fs()
        .create_directory(
            &next_environment.cwd,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: true,
            },
        )
        .await?;
    let next_marker = next_environment.cwd.join("marker.txt")?;

    let paused = start_paused_turn(&test.codex).await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            environments: Some(TurnEnvironmentRequests::new(
                test.config.cwd.join("future-environment"),
                vec![next_environment.into_request()],
            )),
            ..Default::default()
        },
    )
    .await?;
    apply_turn_settings(
        &test.codex,
        &paused.turn_id,
        TurnSettingsUpdate {
            model: Some(MODEL_B.to_string()),
            ..Default::default()
        },
    )
    .await?;
    answer_paused_turn(&test.codex, &paused.turn_id).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_text_turn("write in the newly selected environment")
        .await?;

    assert_eq!(
        responses
            .requests()
            .iter()
            .map(|request| request.body_json()["model"].clone())
            .collect::<Vec<_>>(),
        [MODEL_A, MODEL_B, MODEL_B, MODEL_A, MODEL_A].map(|model| json!(model)),
    );
    assert_eq!(
        (
            test.fs()
                .read_file_text(&test.workspace_path_uri("marker.txt")?, Default::default())
                .await?,
            test.fs()
                .read_file_text(&next_marker, Default::default())
                .await?,
        ),
        ("active step\n".to_string(), "next turn\n".to_string()),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combined_active_model_and_environment_updates_apply_or_reject_together() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            paused_response("pause-before-invalid-update", "pause"),
            paused_response("pause-before-valid-update", "pause-again"),
            sse(vec![
                ev_response_created("write-in-new-environment"),
                ev_apply_patch_custom_tool_call(
                    "write-in-new-environment",
                    "*** Begin Patch\n*** Add File: marker.txt\n+selected B\n*** End Patch\n",
                ),
                ev_completed("write-in-new-environment"),
            ]),
            sse_completed("after-valid-update"),
        ],
    )
    .await;
    let test = direct_tool_settings_test()
        .build_with_auto_env(&server)
        .await?;
    let original = test.executor_environment().selection().clone();
    let mut next_environment = original.clone();
    next_environment.cwd = original.cwd.join("selected-environment")?;
    next_environment.workspace_roots = vec![next_environment.cwd.clone()];
    next_environment.config = EnvironmentConfigState::Ready(environment_config_for_selection(
        &test.config,
        &next_environment,
    ));
    test.fs()
        .create_directory(
            &next_environment.cwd,
            CreateDirectoryOptions {
                recursive: false,
                follow_symlinks: true,
            },
        )
        .await?;
    let next_marker = next_environment.cwd.join("marker.txt")?;
    let next_cwd = next_environment.cwd.clone();

    let paused = start_paused_turn(&test.codex).await?;
    let mut owned_environment = original.clone();
    owned_environment.config =
        EnvironmentConfigState::Ready(environment_config_for_selection(&test.config, &original));
    apply_turn_settings(
        &test.codex,
        &paused.turn_id,
        TurnSettingsUpdate {
            environments: Some(vec![owned_environment.into_request()]),
            ..Default::default()
        },
    )
    .await?;
    let mut inherited_environment = next_environment.clone();
    inherited_environment.config = EnvironmentConfigState::FromThread;
    let inherited = submit_turn_settings(
        &test.codex,
        &paused.turn_id,
        TurnSettingsUpdate {
            model: Some(MODEL_B.to_string()),
            environments: Some(vec![inherited_environment.into_request()]),
            ..Default::default()
        },
    )
    .await?;
    let TurnSettingsUpdateOutcome::Rejected { reason } = inherited else {
        anyhow::bail!(
            "an owner-configured environment cannot go back to inheriting: {inherited:?}"
        );
    };
    assert!(
        reason.contains("owner-provided environment configuration"),
        "{reason}"
    );
    answer_paused_turn(&test.codex, &paused.turn_id).await?;
    let paused = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    apply_turn_settings(
        &test.codex,
        &paused.turn_id,
        TurnSettingsUpdate {
            model: Some(MODEL_B.to_string()),
            environments: Some(vec![next_environment.into_request()]),
            ..Default::default()
        },
    )
    .await?;
    answer_paused_turn(&test.codex, &paused.turn_id).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body_json()["model"].clone())
            .collect::<Vec<_>>(),
        [MODEL_A, MODEL_A, MODEL_B, MODEL_B].map(|model| json!(model)),
    );
    let context = requests[1].message_input_texts("user");
    let original_cwd = format!("<cwd>{}</cwd>", original.cwd.inferred_native_path_string());
    let rejected_cwd = format!("<cwd>{}</cwd>", next_cwd.inferred_native_path_string());
    assert!(context.iter().any(|text| text.contains(&original_cwd)));
    assert!(!context.iter().any(|text| text.contains(&rejected_cwd)));
    assert_eq!(
        test.fs()
            .read_file_text(&next_marker, Default::default())
            .await?,
        "selected B\n",
    );
    Ok(())
}

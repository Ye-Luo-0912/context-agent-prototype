use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    ContextEngine, ContextKind, RuntimeEvent, RuntimeEventEnvelope, ToolSurfaceBlockReason,
    ToolSurfaceDemand, ToolSurfaceOmissionReason, ToolSurfacePlanStatus, ToolSurfaceRequirement,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, approx_layer_tokens, spawn_runtime};

use crate::harness::*;

#[tokio::test]
async fn final_guard_trims_to_the_input_budget_not_the_window() {
    // Window 10_000, output reserve 4_000 -> max input budget 6_000. Three
    // large context items assemble to ~9_000 + fixed layers, so the guard
    // must trim (the old guard compared against the window and let the
    // request eat into the output reserve).
    let model = Arc::new(RecordingModel::default());
    let context = Arc::new(BigContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        model.clone(),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the turn must send exactly one request");
        let request = &requests[0];
        let total = approx_layer_tokens(&request.messages) + approx_layer_tokens(&request.tools);
        assert!(
            total <= 6_000,
            "the assembled request must fit window - output_reserve (got {total})"
        );
        assert!(
            total > 3_000,
            "the guard must trim, not empty the context frame (got {total})"
        );
    }
    {
        let acks = context.acks.lock().unwrap();
        assert_eq!(acks.len(), 1);
        assert!(
            !acks[0].item_ids.is_empty() && acks[0].item_ids.len() < 3,
            "the ack must name the final post-trim subset, got {:?}",
            acks[0].item_ids
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn send_guard_uses_provider_window_pack_uses_kernel_cap() {
    // Kernel pack 24k, provider send 50k, output reserve 4k → input 46k.
    // Append-like engine returns ~36k of history. Coupled 24k send would
    // trim below 24k; a competent A baseline must keep the larger send.
    let model = Arc::new(RecordingModel {
        context_window: 50_000,
        max_output_tokens: 4_000,
        ..RecordingModel::default()
    });
    let context = Arc::new(BigContextEngine::with_items(12));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        model.clone(),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the turn must send exactly one request");
        let request = &requests[0];
        let total = approx_layer_tokens(&request.messages) + approx_layer_tokens(&request.tools);
        assert!(
            total > 24_000,
            "A must be allowed to grow past the 24k pack cap when the send window is larger (got {total})"
        );
        assert!(
            total <= 46_000,
            "the send guard is still window - output_reserve (got {total})"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn final_guard_omits_optional_schema_without_unloading_it() {
    let model = Arc::new(VariableWindowModel::new(1_600));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();

    handle.user_message("small budget".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    let first_surface = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the small-budget round must complete");
        let total =
            approx_layer_tokens(&requests[0].messages) + approx_layer_tokens(&requests[0].tools);
        assert!(
            total <= 1_088,
            "the small round must fit context_window - output_reserve (got {total})"
        );
        let names: Vec<_> = requests[0]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(
            names.contains(&"core.read"),
            "core schema must be preserved"
        );
        assert!(
            !names.contains(&"optional.large"),
            "optional schema must be omitted from the small round"
        );
    }
    assert!(
        tools.optional_loaded(),
        "round-local omission must leave the catalog entry Loaded"
    );
    assert_eq!(tools.unload_calls(), 0, "the actor must never call unload");
    assert_eq!(tools.generation(), 17, "catalog generation must not change");
    assert!(first_surface.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.reason == ToolSurfaceOmissionReason::ProviderInputBudget
    }));

    model.set_context_window(16_000);
    handle.user_message("restored budget".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    let second_surface = wait_for_ready_surface_and_model_start(&mut surface_events).await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the restored-budget round must complete");
        let names: Vec<_> = requests[1]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(names.contains(&"core.read"));
        assert!(
            names.contains(&"optional.large"),
            "the still-loaded optional schema must reappear when budget returns"
        );
    }
    assert!(tools.optional_loaded());
    assert_eq!(tools.unload_calls(), 0);
    assert_eq!(tools.generation(), 17);
    assert!(
        second_surface.surface_revision > first_surface.surface_revision,
        "every successful round needs a unique monotonic surface revision"
    );
    assert_eq!(
        first_surface.source_revisions.builtin_catalog_generation,
        second_surface.source_revisions.builtin_catalog_generation,
        "budget recovery must not masquerade as catalog mutation"
    );
    assert!(
        second_surface
            .selected
            .iter()
            .any(|row| row.tool_name == "optional.large")
    );

    handle.stop().await.unwrap();
}

#[tokio::test]
async fn keep_ready_reloads_after_gc_without_entering_the_model_surface() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::evicting_on_gc());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut turn_events = handle.subscribe();
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("keep the large tool ready".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    let revision = handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::KeepReady,
                reason: "likely needed after the current step".into(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    handle.user_message("run one round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    let report = wait_for_ready_surface_and_model_start(&mut surface_events).await;

    assert_eq!(tools.load_calls(), 1, "Task demand must repair GC eviction");
    assert!(
        tools.optional_loaded(),
        "KeepReady leaves the catalog loaded"
    );
    assert!(
        report
            .selected
            .iter()
            .all(|row| row.tool_name != "optional.large")
    );
    assert!(report.omitted.iter().any(|row| {
        row.tool_name == "optional.large"
            && row.demand == ToolSurfaceDemand::KeepReady
            && row.reason == ToolSurfaceOmissionReason::KeepReady
    }));
    assert_eq!(report.source_revisions.task_requirement_revision, Some(1));
    assert!(report.source_revisions.focus_revision.is_some());
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .tools
                .iter()
                .all(|spec| spec.name != "optional.large"),
            "KeepReady is a lifecycle root, not a prompt-visibility request"
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn active_task_tool_demand_reaches_gc_as_roots() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut turn_events = handle.subscribe();
    handle.start().await.unwrap();

    // Before any task exists, the runtime passes no roots.
    handle
        .user_message("first round, no requirements yet".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    assert!(
        tools.roots_seen().last().unwrap().is_empty(),
        "no active task means no tool roots"
    );

    // A task that requires a tool must hand that tool's name to gc() as a
    // root on every later round — idle GC cannot age it off the surface.
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "the task needs the large tool".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("rooted round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    let seen = tools.roots_seen();
    let roots = seen.last().unwrap();
    assert!(
        roots.iter().any(|root| root == "optional.large"),
        "the task-demanded tool must be a gc root, got {roots:?}"
    );

    // Suspending the task drops the roots again (no active task demand).
    handle.suspend_task().await.unwrap();
    handle.user_message("taskless round".into()).await.unwrap();
    wait_for_turn_completed(&mut turn_events).await;
    assert!(
        tools.roots_seen().last().unwrap().is_empty(),
        "a suspended task must not keep rooting tools"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn must_surface_overflow_is_unsatisfiable_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(1_600));
    let tools = Arc::new(RoundLocalToolDispatcher::new());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    let mut all_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("must use the large tool".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "the task cannot proceed without it".into(),
            }],
        )
        .await
        .unwrap();

    handle
        .user_message("try the constrained round".into())
        .await
        .unwrap();
    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::ProviderInputBudget,
        }
    );
    assert!(report.blocked.iter().any(|row| {
        row.tool_name == "optional.large" && row.demand == ToolSurfaceDemand::MustSurface
    }));

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut saw_model_started = false;
    while let Ok(envelope) = all_events.try_recv() {
        saw_model_started |= matches!(envelope.event, RuntimeEvent::ModelStarted { .. });
    }
    assert!(
        !saw_model_started,
        "an unsatisfiable round must not claim the model started"
    );
    assert!(
        model.requests.lock().unwrap().is_empty(),
        "an unsatisfiable MustSurface requirement must never reach the provider"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unavailable_must_surface_is_reported_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("missing required tool".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "missing.tool".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise unavailable handling".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::Unavailable,
        }
    );
    assert!(
        report
            .blocked
            .iter()
            .any(|row| row.tool_name == "missing.tool")
    );
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn mandatory_schema_cap_is_reported_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(32_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::schema_overflow()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut surface_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("oversized required schema".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise schema cap handling".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let report = wait_for_surface_plan(&mut surface_events).await;
    assert_eq!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable {
            reason: ToolSurfaceBlockReason::SchemaBudget,
        }
    );
    assert!(report.mandatory_schema_tokens > agent_runtime::budget::MAX_TOOL_SURFACE_TOKENS);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn surface_event_failure_aborts_before_model_start_and_provider_call() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailSurfaceEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("exercise event fence".into())
        .await
        .unwrap();

    let saw_model_started = tokio::time::timeout(Duration::from_secs(3), async {
        let mut saw_model_started = false;
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::Error { .. },
                    ..
                }) => break saw_model_started,
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => saw_model_started = true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before surfacing the journal failure")
                }
            }
        }
    })
    .await
    .expect("surface event failure was not surfaced");

    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn consumption_event_failure_rolls_back_reinforcement_and_aborts_the_turn() {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailConsumptionEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .pin("journal-protected constraint".into())
        .await
        .unwrap();
    handle
        .user_message("exercise context ack rollback".into())
        .await
        .unwrap();

    let (saw_consumed, saw_assistant, saw_turn_completed) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut saw_consumed = false;
            let mut saw_assistant = false;
            let mut saw_turn_completed = false;
            loop {
                match events.recv().await {
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::ContextConsumed { .. },
                        ..
                    }) => saw_consumed = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::AssistantMessage { .. },
                        ..
                    }) => saw_assistant = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::TurnCompleted,
                        ..
                    }) => saw_turn_completed = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::Error { message },
                        ..
                    }) if message.contains("failed to commit model context consumption") => {
                        break (saw_consumed, saw_assistant, saw_turn_completed);
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("runtime closed before surfacing the consumption journal failure")
                    }
                }
            }
        })
        .await
        .expect("context-consumption journal failure was not surfaced");

    assert!(!saw_consumed, "a failed append must not be broadcast");
    assert!(
        !saw_assistant,
        "the failed consumption commit must abort output commit"
    );
    assert!(!saw_turn_completed, "the failed turn must not commit");
    let pinned = context
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.kind == ContextKind::Constraint)
        .expect("pinned constraint survives rollback");
    assert_eq!(
        pinned.access_count, 0,
        "the checkpoint rollback must remove the unjournaled reinforcement"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unsatisfiable_surface_event_failure_is_reported_and_provider_fenced() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailSurfaceEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .set_focus("missing required tool with a failing journal".into())
        .await
        .unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "missing.tool".into(),
                demand: ToolSurfaceDemand::MustSurface,
                reason: "exercise the unsatisfiable audit fence".into(),
            }],
        )
        .await
        .unwrap();
    handle.user_message("start".into()).await.unwrap();

    let (message, saw_model_started) = tokio::time::timeout(Duration::from_secs(3), async {
        let mut saw_model_started = false;
        loop {
            match events.recv().await {
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::Error { message },
                    ..
                }) if message
                    .contains("failed to persist the unavailable-tool surface decision") =>
                {
                    break (message, saw_model_started);
                }
                Ok(RuntimeEventEnvelope {
                    event: RuntimeEvent::ModelStarted { .. },
                    ..
                }) => saw_model_started = true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("runtime closed before surfacing the unsatisfiable journal failure")
                }
            }
        }
    })
    .await
    .expect("unsatisfiable surface event failure was not surfaced");

    assert!(message.contains("simulated surface-plan journal failure"));
    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn final_guard_refuses_an_unshrinkable_over_budget_request() {
    // Window 1_000, output reserve 2_000 -> max input budget 0. The fixed
    // layers alone (system prompt, turn frame, mandatory tool schema)
    // overshoot, so no amount of round-local omission helps: refuse to
    // send instead of silently over-budgeting the provider.
    let model = Arc::new(TinyWindowModel::default());
    let context = Arc::new(BigContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        model.clone(),
        Arc::new(OneToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel.clone());
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    let mut saw_error = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::Error { .. }) {
                saw_error = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saw_error,
        "an unshrinkable over-budget request must surface a hard error"
    );
    assert_eq!(
        model.calls(),
        0,
        "an over-budget request must never reach the provider"
    );
    assert!(
        context.acks.lock().unwrap().is_empty(),
        "a refused provider request must not commit context consumption"
    );
    handle.stop().await.unwrap();
}

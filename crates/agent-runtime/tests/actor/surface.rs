use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ContextEngine, ContextKind, RuntimeEvent, RuntimeEventEnvelope,
    RuntimeFailureClass, ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolLeaseBoundary,
    ToolLifecycle, ToolOutcome, ToolRisk, ToolSpec, ToolSurfaceBlockReason, ToolSurfaceDemand,
    ToolSurfaceOmissionReason, ToolSurfacePlanStatus, ToolSurfaceRequirement,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, approx_layer_tokens, spawn_runtime};

use crate::harness::*;

#[derive(Debug, Default)]
struct IntentGatedCompletionDispatcher {
    loaded: AtomicBool,
}

impl IntentGatedCompletionDispatcher {
    fn spec() -> ToolSpec {
        ToolSpec {
            name: "task.complete".into(),
            description: "close the active task".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["summary"],
                "properties": {"summary": {"type": "string"}}
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for IntentGatedCompletionDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        self.loaded
            .load(Ordering::SeqCst)
            .then(Self::spec)
            .into_iter()
            .collect()
    }

    fn may_omit_from_round(&self, name: &str) -> bool {
        name == "task.complete"
    }

    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        vec![ToolCatalogEntry {
            name: "task.complete".into(),
            state: if self.loaded.load(Ordering::SeqCst) {
                ToolLifecycle::Loaded
            } else {
                ToolLifecycle::Available
            },
            owner: "builtin".into(),
            description: "close the active task".into(),
            risk: ToolRisk::ReadOnly,
            roles: Vec::new(),
        }]
    }

    fn load_tool(&self, name: &str) -> AgentResult<()> {
        if name != "task.complete" {
            return Err(AgentError::InvalidRequest(format!("unknown tool: {name}")));
        }
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn inspect_tool(&self, name: &str) -> Option<ToolSpec> {
        (name == "task.complete").then(Self::spec)
    }

    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(AgentError::InvalidRequest(
            "the recording model never calls tools".into(),
        ))
    }
}

#[tokio::test]
async fn task_completion_surface_is_gated_by_explicit_turn_intent() {
    let model = Arc::new(RecordingModel::default());
    let tools = Arc::new(IntentGatedCompletionDispatcher::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();

    handle
        .user_message("continue implementing the current task".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;
    handle.user_message("mark this done".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .tools
                .iter()
                .all(|spec| spec.name != "task.complete"),
            "ordinary turn completion must preserve task continuity"
        );
        assert!(
            requests[1]
                .tools
                .iter()
                .any(|spec| spec.name == "task.complete"),
            "explicit task-closure intent must lease the typed completion tool"
        );
    }
    handle.stop().await.unwrap();
}

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
async fn oversized_materialization_is_rejected_before_the_provider() {
    // A 257-item append-only round through an adapter that ignores the
    // query caps must be rejected by the materialization validator before
    // any provider request, instead of relying on the consumption ACK to
    // discard the oversized frame later.
    let model = Arc::new(RecordingModel::default());
    let context = Arc::new(BigContextEngine::with_items(257));
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
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "the provider must never run for an oversized frame"
    );
    assert!(
        context.acks.lock().unwrap().is_empty(),
        "no frame may reach the consumption ack"
    );
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
async fn directive_boundary_releases_unrooted_optional_schema_before_model_start() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
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
    handle.start().await.unwrap();
    handle.user_message("new directive".into()).await.unwrap();

    let seen = tokio::time::timeout(Duration::from_secs(3), async {
        let mut seen = Vec::new();
        loop {
            let envelope = events.recv().await.unwrap();
            let done = matches!(envelope.event, RuntimeEvent::TurnCompleted);
            seen.push(envelope.event);
            if done {
                break seen;
            }
        }
    })
    .await
    .expect("turn completes");

    let lease_index = seen
        .iter()
        .position(|event| matches!(event, RuntimeEvent::ToolLeasesReconciled { .. }))
        .expect("lease event");
    let surface_index = seen
        .iter()
        .position(|event| matches!(event, RuntimeEvent::ToolSurfacePlanned { .. }))
        .expect("surface event");
    let model_index = seen
        .iter()
        .position(|event| matches!(event, RuntimeEvent::ModelStarted { .. }))
        .expect("model event");
    assert!(lease_index < surface_index && surface_index < model_index);
    match &seen[lease_index] {
        RuntimeEvent::ToolLeasesReconciled {
            boundary, report, ..
        } => {
            assert_eq!(*boundary, ToolLeaseBoundary::DirectiveStart);
            assert_eq!(report.examined_loaded_optional, 1);
            assert_eq!(report.released_to_warm, 1);
            assert_eq!(report.retained_by_root, 0);
        }
        _ => unreachable!(),
    }
    assert!(!tools.optional_loaded());
    assert_eq!(tools.unload_calls(), 0, "lease release is Warm, not unload");
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .tools
                .iter()
                .all(|spec| spec.name != "optional.large")
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn task_requirement_roots_optional_schema_across_directive_reconcile() {
    let model = Arc::new(VariableWindowModel::new(16_000));
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
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
    handle.start().await.unwrap();
    handle.set_focus("rooted capability".into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .replace_task_tool_requirements(
            task_id,
            0,
            vec![ToolSurfaceRequirement {
                tool_name: "optional.large".into(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: "task-scoped capability".into(),
            }],
        )
        .await
        .unwrap();
    handle
        .user_message("use the rooted tool".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    assert!(tools.optional_loaded());
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|spec| spec.name == "optional.large")
        );
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn result_delivery_lease_survives_load_and_call_then_releases_on_non_use() {
    let model = Arc::new(LeaseFlowModel::default());
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
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
    let mut audit_events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("exercise a leased tool".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut turn_events).await;

    let (lease_reports, action_batches) = tokio::time::timeout(Duration::from_secs(3), async {
        let mut reports = Vec::new();
        let mut batches = Vec::new();
        loop {
            let envelope = audit_events.recv().await.unwrap();
            match envelope.event {
                RuntimeEvent::ToolLeasesReconciled {
                    boundary, report, ..
                } => reports.push((boundary, report)),
                RuntimeEvent::ExecutionBatchSettled {
                    requested,
                    terminal,
                    spawned,
                    refused,
                    reused,
                    missing_terminal,
                    unexpected_terminal,
                    ..
                } => batches.push((
                    requested,
                    terminal,
                    spawned,
                    refused,
                    reused,
                    missing_terminal,
                    unexpected_terminal,
                )),
                RuntimeEvent::TurnCompleted => break (reports, batches),
                _ => {}
            }
        }
    })
    .await
    .expect("audit stream reaches turn completion");

    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert_eq!(tools.load_calls(), 1);
    assert_eq!(
        action_batches,
        vec![(1, 1, 1, 0, 0, 0, 0), (1, 1, 1, 0, 0, 0, 0)],
        "each model-requested action must receive exactly one terminal disposition"
    );
    assert!(
        lease_reports.iter().any(|(boundary, report)| {
            *boundary == ToolLeaseBoundary::ModelDecision
                && report.retained_by_root == 1
                && report.released_to_warm == 0
        }),
        "the call decision must renew the result-delivery lease"
    );
    assert!(
        lease_reports.iter().any(|(boundary, report)| {
            *boundary == ToolLeaseBoundary::ModelDecision
                && report.released_to_warm == 1
                && report.released_tools == vec!["optional.large"]
        }),
        "the first successful decision that does not reuse the tool releases it"
    );
    assert!(
        !tools.optional_loaded(),
        "released target leaves the model surface"
    );
    assert_eq!(tools.unload_calls(), 0, "release remains a Warm transition");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn pending_load_cohort_survives_adjacent_loads_until_each_tool_is_used() {
    let model = Arc::new(CohortLeaseModel::default());
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
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
    handle.start().await.unwrap();
    handle
        .user_message("assemble and use an optional tool cohort".into())
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    assert_eq!(model.requests.lock().unwrap().len(), 5);
    assert_eq!(tools.load_calls(), 2, "each cohort member loads once");
    assert!(!tools.optional_loaded(), "used A releases at turn end");
    assert!(!tools.optional_peer_loaded(), "used B releases at turn end");
    assert_eq!(tools.unload_calls(), 0, "release remains Warm, not unload");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn oversized_provider_batch_is_refused_without_dispatch_and_fully_accounted() {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(OversizedToolBatchModel),
        Arc::new(RoundLocalToolDispatcher::new()),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("return an oversized action batch".into())
        .await
        .unwrap();

    let mut saw_limit_error = false;
    let mut saw_tool_started = false;
    let batch = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await.unwrap().event {
                RuntimeEvent::Error { message } => {
                    saw_limit_error |= message.contains("hard safety limit");
                }
                RuntimeEvent::ToolStarted { .. } => saw_tool_started = true,
                RuntimeEvent::ExecutionBatchSettled {
                    requested,
                    terminal,
                    spawned,
                    refused,
                    transient_no_persist,
                    failed,
                    no_outcome_results,
                    missing_terminal,
                    unexpected_terminal,
                    ..
                } => {
                    break (
                        requested,
                        terminal,
                        spawned,
                        refused,
                        transient_no_persist,
                        failed,
                        no_outcome_results,
                        missing_terminal,
                        unexpected_terminal,
                    );
                }
                _ => {}
            }
        }
    })
    .await
    .expect("oversized batch reaches a terminal ledger event");

    assert!(saw_limit_error);
    assert!(!saw_tool_started, "no member of an oversized batch may run");
    assert_eq!(
        batch,
        (
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            0,
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            agent_contracts::MAX_MODEL_TOOL_CALLS_PER_ROUND + 1,
            0,
            0,
        ),
        "batch-level refusal must terminalize every requested action exactly once"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn speculative_missing_path_is_reused_only_after_live_workspace_check() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
    let tools = Arc::new(MissingReadDispatcher::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(DuplicateMissingReadModel::default()),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
    .with_artifact_workspace(Arc::new(workspace));
    let (handle, _task) = spawn_runtime(Arc::new(services));
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("inspect a possible implementation location".into())
        .await
        .unwrap();

    let (started, finished, recorded, reused, batch) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut started = 0;
            let mut finished = 0;
            let mut recorded = 0;
            let mut reused = 0;
            let mut batch = None;
            loop {
                match events.recv().await.unwrap().event {
                    RuntimeEvent::ToolStarted { call } if call.name == "fs.read" => {
                        started += 1;
                    }
                    RuntimeEvent::ToolFinished { output, .. } if output.tool_name == "fs.read" => {
                        finished += 1;
                    }
                    RuntimeEvent::ExecutionNegativeFact { kind, .. } => match kind {
                        agent_contracts::NegativeFactEventKind::Recorded => recorded += 1,
                        agent_contracts::NegativeFactEventKind::Reused => reused += 1,
                        _ => {}
                    },
                    RuntimeEvent::ExecutionBatchSettled {
                        requested,
                        terminal,
                        spawned,
                        reused,
                        ..
                    } => batch = Some((requested, terminal, spawned, reused)),
                    RuntimeEvent::TurnCompleted => {
                        break (started, finished, recorded, reused, batch);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("duplicate missing-path turn completes");

    assert_eq!(
        tools.executions.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(started, 1, "only the first call may dispatch");
    assert_eq!(finished, 2, "both model calls receive terminal results");
    assert_eq!(recorded, 1);
    assert_eq!(reused, 1);
    assert_eq!(batch, Some((2, 2, 1, 1)));
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn exact_current_verification_pass_avoids_only_the_equivalent_dispatch() {
    let tools = Arc::new(ExactVerifyDispatcher::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(DuplicateExactVerifyModel::default()),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let (handle, _task) = spawn_runtime(Arc::new(services));
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("run one deterministic verification recipe".into())
        .await
        .unwrap();

    let (started, finished, recorded, reused, reused_output, batch) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut started = 0;
            let mut finished = 0;
            let mut recorded = 0;
            let mut reused = 0;
            let mut reused_output = false;
            let mut batch = None;
            loop {
                match events.recv().await.unwrap().event {
                    RuntimeEvent::ToolStarted { call } if call.name == "test.verify" => {
                        started += 1;
                    }
                    RuntimeEvent::ToolFinished { output, .. }
                        if output.tool_name == "test.verify" =>
                    {
                        finished += 1;
                        reused_output |= output
                            .metadata
                            .get("verification_pass_reused")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                    }
                    RuntimeEvent::ExecutionVerificationPass { kind, .. } => match kind {
                        agent_contracts::VerificationPassEventKind::Recorded => recorded += 1,
                        agent_contracts::VerificationPassEventKind::Reused => reused += 1,
                    },
                    RuntimeEvent::ExecutionBatchSettled {
                        requested,
                        terminal,
                        spawned,
                        reused,
                        ..
                    } => batch = Some((requested, terminal, spawned, reused)),
                    RuntimeEvent::TurnCompleted => {
                        break (started, finished, recorded, reused, reused_output, batch);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("duplicate exact-verification turn completes");

    assert_eq!(
        tools.executions.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(started, 1, "only the first verifier call may dispatch");
    assert_eq!(finished, 2, "both requested calls receive terminal results");
    assert_eq!(recorded, 1);
    assert_eq!(reused, 1);
    assert!(
        reused_output,
        "the skipped call must disclose executed=false reuse"
    );
    assert_eq!(batch, Some((2, 2, 1, 1)));
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn exact_verification_identity_drift_executes_again_and_marks_the_result() {
    let tools = Arc::new(DriftingExactVerifyDispatcher::default());
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(DuplicateExactVerifyModel::default()),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let (handle, _task) = spawn_runtime(Arc::new(services));
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("run a verifier while its identity changes".into())
        .await
        .unwrap();

    let (started, recorded, reused, drifted, batch) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut started = 0;
            let mut recorded = 0;
            let mut reused = 0;
            let mut drifted = 0;
            let mut batch = None;
            loop {
                match events.recv().await.unwrap().event {
                    RuntimeEvent::ToolStarted { call } if call.name == "test.verify" => {
                        started += 1;
                    }
                    RuntimeEvent::ToolFinished { output, .. }
                        if output.tool_name == "test.verify" =>
                    {
                        drifted += usize::from(
                            output
                                .metadata
                                .get("verification_identity_stable")
                                .and_then(serde_json::Value::as_bool)
                                == Some(false),
                        );
                    }
                    RuntimeEvent::ExecutionVerificationPass { kind, .. } => match kind {
                        agent_contracts::VerificationPassEventKind::Recorded => recorded += 1,
                        agent_contracts::VerificationPassEventKind::Reused => reused += 1,
                    },
                    RuntimeEvent::ExecutionBatchSettled {
                        requested,
                        terminal,
                        spawned,
                        reused,
                        ..
                    } => batch = Some((requested, terminal, spawned, reused)),
                    RuntimeEvent::TurnCompleted => {
                        break (started, recorded, reused, drifted, batch);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("identity-drift turn completes");

    assert_eq!(
        tools.executions.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert_eq!(started, 2, "identity drift must prevent the second skip");
    assert_eq!(recorded, 1, "only the stable second PASS is exact");
    assert_eq!(reused, 0);
    assert_eq!(drifted, 1, "the downgraded PASS stays event-visible");
    assert_eq!(batch, Some((2, 2, 2, 0)));
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn lease_transition_audit_failure_fences_before_model_start() {
    let model = Arc::new(RecordingModel::default());
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailToolLeaseEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("start a directive".into())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::RecoveryRequired
            ) {
                break;
            }
        }
    })
    .await
    .expect("an unaudited lease transition must fence the actor");

    assert!(model.requests.lock().unwrap().is_empty());
    assert!(
        !tools.optional_loaded(),
        "the transition landed, so failure must fence rather than pretend it rolled back"
    );
    assert!(
        handle.user_message("must be fenced".into()).await.is_err(),
        "a known audit gap requires restore before another directive"
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn action_batch_audit_failure_fences_before_following_model_decision() {
    let model = Arc::new(LeaseFlowModel::default());
    let tools = Arc::new(RoundLocalToolDispatcher::lease_reconciling());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        model.clone(),
        tools.clone(),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailActionBatchEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("load a leased capability".into())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::RecoveryRequired
            ) {
                break;
            }
        }
    })
    .await
    .expect("an unaudited action batch must fence the actor");

    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(tools.load_calls(), 1, "the first action did execute");
    assert!(
        handle.user_message("must be fenced".into()).await.is_err(),
        "the runtime cannot continue after losing the batch terminal record"
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

/// A deliberate Unsatisfiable refusal is not a commit failure: no
/// `TurnCommitFailed` may be journaled for it, but the applied user input
/// must still settle out of Applied instead of dangling there forever.
#[tokio::test]
async fn unsatisfiable_round_settles_input_and_reports_typed_budget_refusal() {
    use agent_contracts::{InputLifecycle, ToolSurfaceDemand, ToolSurfaceRequirement};

    let model = Arc::new(VariableWindowModel::new(1_600));
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
        .user_message("settle me after the refusal".into())
        .await
        .unwrap();
    let report = wait_for_surface_plan(&mut surface_events).await;
    assert!(matches!(
        report.status,
        ToolSurfacePlanStatus::Unsatisfiable { .. }
    ));

    // The settlement event follows the refusal inside the same actor step.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut settled = false;
    let mut saw_commit_failed = false;
    let mut saw_input_budget = false;
    while let Ok(envelope) = all_events.try_recv() {
        match envelope.event {
            RuntimeEvent::UserMessageAccepted { input } => {
                settled |= input.lifecycle == InputLifecycle::InterruptCommitted;
            }
            RuntimeEvent::TurnCommitFailed { .. } => saw_commit_failed = true,
            RuntimeEvent::Failure { class, .. } => {
                saw_input_budget |= class == RuntimeFailureClass::InputBudget;
            }
            _ => {}
        }
    }
    assert!(
        settled,
        "a refused round must commit the applied input's interruption"
    );
    assert!(
        !saw_commit_failed,
        "a deliberate refusal must not journal a turn-commit failure"
    );
    assert!(saw_input_budget, "the refusal must retain its typed cause");
    assert!(model.requests.lock().unwrap().is_empty());
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

    let (phase, message, saw_recovery_required, saw_model_started) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut commit_failure = None;
            let mut saw_recovery_required = false;
            let mut saw_model_started = false;
            loop {
                match events.recv().await {
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::TurnCommitFailed { phase, message },
                        ..
                    }) => commit_failure = Some((phase, message)),
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::RecoveryRequired,
                        ..
                    }) => saw_recovery_required = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::ModelStarted { .. },
                        ..
                    }) => saw_model_started = true,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("runtime closed before surfacing the journal failure")
                    }
                }
                if saw_recovery_required && let Some((phase, message)) = commit_failure.take() {
                    break (phase, message, saw_recovery_required, saw_model_started);
                }
            }
        })
        .await
        .expect("surface event failure was not surfaced");

    assert_eq!(phase, "tool_surface_planned_event");
    assert!(message.contains("simulated surface-plan journal failure"));
    assert!(saw_recovery_required);
    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    let next = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("a failed surface event must fence later turns");
    assert!(matches!(
        next,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
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

    let (phase, message, saw_recovery_required, saw_model_started) =
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut commit_failure = None;
            let mut saw_recovery_required = false;
            let mut saw_model_started = false;
            loop {
                match events.recv().await {
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::TurnCommitFailed { phase, message },
                        ..
                    }) => commit_failure = Some((phase, message)),
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::RecoveryRequired,
                        ..
                    }) => saw_recovery_required = true,
                    Ok(RuntimeEventEnvelope {
                        event: RuntimeEvent::ModelStarted { .. },
                        ..
                    }) => saw_model_started = true,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("runtime closed before surfacing the unsatisfiable journal failure")
                    }
                }
                if saw_recovery_required && let Some((phase, message)) = commit_failure.take() {
                    break (phase, message, saw_recovery_required, saw_model_started);
                }
            }
        })
        .await
        .expect("unsatisfiable surface event failure was not surfaced");

    assert_eq!(phase, "tool_surface_planned_event");
    assert!(message.contains("simulated surface-plan journal failure"));
    assert!(saw_recovery_required);
    assert!(!saw_model_started);
    assert!(model.requests.lock().unwrap().is_empty());
    let next = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("a failed unsatisfiable-surface event must fence later turns");
    assert!(matches!(
        next,
        agent_contracts::AgentError::RecoveryRequired(_)
    ));
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

    let mut saw_input_budget_failure = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(
                envelope.event,
                RuntimeEvent::Failure {
                    class: RuntimeFailureClass::InputBudget,
                    retryable: false,
                    ..
                }
            ) {
                saw_input_budget_failure = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saw_input_budget_failure,
        "an unshrinkable over-budget request must surface a typed hard failure"
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

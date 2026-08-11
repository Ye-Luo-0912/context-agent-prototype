//! End-to-end recall acceptance test: externalized information is pulled
//! back on demand through the real runtime loop, and the prompt itself
//! never carried that content.
//!
//! The chain under test is the full retrieval surface: the model calls
//! `context.manage op=fetch` → the tool names an `EngineQuery` → the actor
//! routes it to the kernel → the kernel resolves it against the real
//! engine (which reads the context store) → the model receives the exact
//! content in the tool result. Meanwhile the materialized frame exposes
//! externalized items as refs only (uri + summary), so the fetched content
//! rides the tool channel, never the prompt.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, CONTEXT_MANAGE, ContextEngine, ContextIngress, ContextItemId,
    ContextKind, ContextMaintenanceTrigger, ContextQuery, ContextSearchQuery, EngineQuery,
    FocusState, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent, TaskId,
    ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};

use agent_kernel::{AgentKernelConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::{Value, json};

/// Drive the engine into a state with at least one externalized ref, and
/// return (the ref's item id, the exact content the store holds for it).
/// The test learns the content from the store so the assertions hold no
/// matter which of the seeded observations survived externalization.
///
/// The seeded contents are longer than the ref summary preview window
/// (120 chars), so the *full* content can never ride the prompt — only the
/// bounded preview can — and the fetch is the only way it comes back.
async fn seed_externalized(engine: &SimpleContextEngine) -> (ContextItemId, String) {
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "service layer"),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let contents = [
        format!("step 0: fix AuthService.rs {}", "x".repeat(160)),
        format!("step 1: fix AuthService.rs {}", "y".repeat(160)),
    ];
    for (i, content) in contents.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: observation_output(&format!("step-{i}"), true, content),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.externalized >= 1,
        "the seed must externalize at least one item: {report:?}"
    );

    let refs = engine
        .search_external(ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(
        !refs.is_empty(),
        "the seed must leave searchable external refs"
    );
    let target = refs[0].item_id;
    let fetched = engine
        .fetch_external(target)
        .await
        .unwrap()
        .expect("the store must hold the ref it advertises");
    (target, fetched.content)
}

fn observation_output(id: &str, ok: bool, content: &str) -> ToolOutput {
    ToolOutput {
        call_id: id.into(),
        tool_name: "shell.exec".into(),
        ok,
        summary: "ok".into(),
        model_content: content.into(),
        artifact_ref: None,
        metadata: json!({}),
    }
}

/// The minimal retrieval surface: `context.manage` emits the typed
/// `EngineQuery` the runtime resolves for fetch/search/inspect and the
/// typed `ContextAction` it routes for admit/derive — the tool still never
/// touches the engine (invariant 3), it only names what it wants.
#[derive(Debug)]
struct EngineQueryTools;

#[async_trait::async_trait]
impl ToolDispatcher for EngineQueryTools {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: CONTEXT_MANAGE.into(),
            description: "runtime context control and retrieval".into(),
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {"type": "string", "enum": ["fetch", "search", "inspect", "admit", "derive"]},
                    "item_id": {"type": "string"},
                    "reason": {"type": "string"},
                    "fact": {"type": "string"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let op = request
            .call
            .arguments
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("");
        let item_id = request
            .call
            .arguments
            .get("item_id")
            .and_then(Value::as_str);
        let parse_id = || -> AgentResult<ContextItemId> {
            let id =
                item_id.ok_or_else(|| AgentError::InvalidRequest("missing item_id".to_string()))?;
            id.parse()
                .map_err(|error| AgentError::InvalidRequest(format!("bad item id: {error}")))
        };
        match (op, item_id) {
            ("fetch", Some(_)) => {
                let item_id = parse_id()?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: request.call.id.clone(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::FetchExternal { item_id },
                })
            }
            ("search", _) => Ok(ToolOutcome::EngineQuery {
                output: ToolOutput {
                    call_id: request.call.id.clone(),
                    tool_name: CONTEXT_MANAGE.into(),
                    ok: true,
                    summary: "querying the context engine".into(),
                    model_content: "resolving...".into(),
                    artifact_ref: None,
                    metadata: json!({"engine_query": true}),
                },
                query: EngineQuery::SearchExternal {
                    query: "step".into(),
                    kind: None,
                    scope: None,
                    task_id: None,
                    limit: 16,
                },
            }),
            ("inspect", Some(_)) => {
                let item_id = parse_id()?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: request.call.id.clone(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::InspectExternal { item_id },
                })
            }
            ("admit", Some(_)) => {
                let item_id = parse_id()?;
                let reason = request
                    .call
                    .arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("needed again")
                    .to_string();
                let action = agent_contracts::ContextAction::Admit { item_id, reason };
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: request.call.id.clone(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "admitting ref".into(),
                        model_content: "admitting ref".into(),
                        artifact_ref: None,
                        metadata: json!({"context_action": "admit"}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            ("derive", Some(_)) => {
                let item_id = parse_id()?;
                let fact = request
                    .call
                    .arguments
                    .get("fact")
                    .and_then(Value::as_str)
                    .unwrap_or("the fix landed in AuthService.rs")
                    .to_string();
                let reason = request
                    .call
                    .arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("lesson")
                    .to_string();
                let action = agent_contracts::ContextAction::Derive {
                    item_id,
                    fact,
                    reason,
                };
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: request.call.id.clone(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "deriving fact".into(),
                        model_content: "deriving fact".into(),
                        artifact_ref: None,
                        metadata: json!({"context_action": "derive"}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            other => Err(AgentError::Tool(format!("unsupported op: {other:?}"))),
        }
    }
}

/// Two-round scripted model. Round 1 must see the externalized ref in the
/// prompt (and never the content); it answers with a fetch tool call.
/// Round 2 must see the exact content in the tool result; it replies plain
/// so the turn finalizes. Observations are recorded instead of panicking
/// inside the model so a regression fails the assertions, not the task.
struct RecallModel {
    target_id: ContextItemId,
    expected_content: String,
    round_1: Mutex<Vec<String>>,
    round_1_saw_content: AtomicBool,
    round_2_saw_content: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RecallModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.calls.fetch_add(1, Ordering::SeqCst);
        let all_text: String = request
            .messages
            .iter()
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let ref_lines: Vec<String> = all_text
            .lines()
            .filter(|line| line.contains("context://run/"))
            .map(str::to_string)
            .collect();

        if round == 0 {
            // Before the fetch the prompt shows the ref with only its
            // bounded preview — never the full content, which is longer
            // than the preview window. That is the "refs only" half of
            // the lifecycle.
            self.round_1.lock().unwrap().extend(ref_lines);
            if all_text.contains(&self.expected_content) {
                self.round_1_saw_content.store(true, Ordering::SeqCst);
            }
            return Ok(ModelOutput {
                content: "pulling the externalized observation back".into(),
                tool_calls: vec![
                    ToolCall {
                        id: "recall-1".into(),
                        name: CONTEXT_MANAGE.into(),
                        arguments: json!({"op": "fetch", "item_id": self.target_id.to_string()}),
                    },
                    // Search and inspect are transient reads too: they must
                    // not duplicate the ref under a new observation id.
                    ToolCall {
                        id: "recall-2".into(),
                        name: CONTEXT_MANAGE.into(),
                        arguments: json!({"op": "search", "query": "step"}),
                    },
                    ToolCall {
                        id: "recall-3".into(),
                        name: CONTEXT_MANAGE.into(),
                        arguments: json!({"op": "inspect", "item_id": self.target_id.to_string()}),
                    },
                ],
                usage: Default::default(),
            });
        }

        // Round 2: the tool result carries the exact content back.
        if all_text.contains(&self.expected_content) {
            self.round_2_saw_content.store(true, Ordering::SeqCst);
        }
        Ok(ModelOutput {
            content: "recalled the observation".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn recall_turn_pulls_external_content_back_without_polluting_the_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        // The seed's user message sits in a suspended task scope after the
        // runtime auto-creates its own task; a long TTL + generation cap
        // keep it resident so the count assertions measure the turn's
        // disposition behavior, not the generational GC's staleness.
        turn_ttl_ticks: 1000,
        gc_max_generation: 100,
        ..SimpleContextConfig::default()
    }));
    let (target_id, expected_content) = seed_externalized(&engine).await;
    let catalog_before = engine.diagnostics().await.unwrap().total_items;

    // The prompt never carried the full content: materialize right after
    // the seed and confirm the frame exposes the externalized item as a
    // ref (uri + bounded preview), while the working set holds none of
    // its content.
    let before = engine
        .materialize(ContextQuery {
            current_input: "recall the observation".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        before
            .external
            .iter()
            .any(|entry| entry.context_ref.uri.ends_with(&target_id.to_string())),
        "the materialized frame must expose the externalized ref"
    );
    assert!(
        before
            .items
            .iter()
            .all(|item| !item.content.contains(&expected_content)),
        "externalized content must not ride in the working set"
    );

    // The runtime loop pulls it back on demand: model -> tool -> engine
    // query -> kernel -> real engine store read -> model tool result.
    let model = Arc::new(RecallModel {
        target_id,
        expected_content: expected_content.clone(),
        round_1: Mutex::new(Vec::new()),
        round_1_saw_content: AtomicBool::new(false),
        round_2_saw_content: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let kernel = Arc::new(RuntimeServices::new(
        AgentKernelConfig::default(),
        engine.clone(),
        model.clone(),
        Arc::new(EngineQueryTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("recall the step-1 observation".into())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    let mut reactivated = 0usize;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TurnCompleted => completed = true,
                RuntimeEvent::ContextGc { report } => reactivated += report.reactivated,
                _ => {}
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the recall turn must complete");
    handle.stop().await.unwrap();

    // The model saw the ref in the prompt, fetched it, and got the exact
    // content back — through the tool result, not the prompt.
    {
        let round_1 = model.round_1.lock().unwrap();
        assert!(
            round_1
                .iter()
                .any(|line| line.contains(&target_id.to_string())),
            "round 1 must expose the externalized ref, saw: {round_1:?}"
        );
    }
    assert!(
        !model.round_1_saw_content.load(Ordering::SeqCst),
        "the prompt must not carry the full externalized content before the fetch"
    );
    assert!(
        model.round_2_saw_content.load(Ordering::SeqCst),
        "the model must receive the exact content through the tool result"
    );

    // Fetch/search/inspect results are transient. They reached the model
    // through the turn frame, but finalization must not persist them as
    // new observations — the turn's own user + assistant messages are
    // expected to persist, and a retrieval result would show up as an
    // extra ToolObservation. The mid-turn WorkingSetSignal, however, made
    // the fetched content's entities hot again, so the turn-boundary GC
    // *recalls* the seeded entries (same ids, from warm/cold back to the
    // resident heap) — that is the CTX-08 behavior, not a new observation.
    let after = engine.diagnostics().await.unwrap();
    let summaries = engine.inspect(100).await.unwrap();
    assert_eq!(
        after.total_items,
        catalog_before + 2,
        "the turn adds its user + assistant messages; the recalled entries \
         were already part of the logical catalog (warm/cold -> resident is \
         a location move, not a new item), got {} -> {}",
        catalog_before,
        after.total_items
    );
    assert!(
        reactivated >= 1,
        "the signaled entities must recall the seeded warm/cold evidence"
    );
    // The ToolObservations in the catalog are the *seeded* ones — recalled
    // with their original ids, not new items. A fetch/search/inspect result
    // persisted as a fresh observation would carry the turn's own
    // `created_turn`; every ToolObservation predates it.
    let user_turn = summaries
        .iter()
        .filter(|s| s.kind == ContextKind::UserMessage)
        .map(|s| s.created_turn)
        .max()
        .expect("the turn's user message is present");
    assert!(
        summaries
            .iter()
            .filter(|s| s.kind == ContextKind::ToolObservation)
            .all(|s| s.created_turn < user_turn),
        "context fetch/search/inspect must not persist new ToolObservations: {:?}",
        summaries
            .iter()
            .filter(|s| s.kind == ContextKind::ToolObservation)
            .map(|s| (s.created_turn, s.source.as_deref()))
            .collect::<Vec<_>>()
    );
    // The signaled entities recalled the seeded entry into the working set
    // (same id, warm/cold -> resident — the CTX-08 acceptance). It is no
    // longer an external ref; it must be resident with its original id.
    assert!(
        summaries.iter().any(|s| s.id == target_id),
        "the recalled entry must be resident with its original id"
    );
    assert!(
        engine.inspect_external(target_id).await.unwrap().is_none(),
        "a recalled entry leaves the external map (it lives in the heap again)"
    );
}

/// End to end for the directive half: `admit` re-enters the item under its
/// ORIGINAL id (identity preserved, one lifecycle transition) and `derive`
/// mints a new Note — and neither directive's tool result is duplicated as
/// a ToolObservation, because the admission event and the derived item are
/// the records.
#[tokio::test]
async fn admit_and_derive_through_the_runtime_never_duplicate_observations() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        // Same long-TTL/generation choice as the fetch test: the count
        // assertions must measure dispositions, not GC staleness.
        turn_ttl_ticks: 1000,
        gc_max_generation: 100,
        ..SimpleContextConfig::default()
    }));
    let (target_id, _) = seed_externalized(&engine).await;
    let catalog_before = engine.diagnostics().await.unwrap().total_items;

    // One turn: round 1 calls admit (same id) + derive (new id) on the ref;
    // round 2 replies plain so the turn finalizes.
    struct AdmitDeriveModel {
        target_id: ContextItemId,
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ModelTransport for AdmitDeriveModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            let round = self.calls.fetch_add(1, Ordering::SeqCst);
            if round == 0 {
                return Ok(ModelOutput {
                    content: "admitting and deriving from the ref".into(),
                    tool_calls: vec![
                        ToolCall {
                            id: "admit-1".into(),
                            name: CONTEXT_MANAGE.into(),
                            arguments: json!({
                                "op": "admit",
                                "item_id": self.target_id.to_string(),
                                "reason": "the model needs this step again"
                            }),
                        },
                        ToolCall {
                            id: "derive-1".into(),
                            name: CONTEXT_MANAGE.into(),
                            arguments: json!({
                                "op": "derive",
                                "item_id": self.target_id.to_string(),
                                "fact": "the auth fix landed in AuthService.rs",
                                "reason": "lesson"
                            }),
                        },
                    ],
                    usage: Default::default(),
                });
            }
            Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }

    let model = Arc::new(AdmitDeriveModel {
        target_id,
        calls: AtomicUsize::new(0),
    });
    let kernel = Arc::new(RuntimeServices::new(
        AgentKernelConfig::default(),
        engine.clone(),
        model.clone(),
        Arc::new(EngineQueryTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _runtime_task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle
        .user_message("admit and derive from the recalled ref".into())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                completed = true;
            }
        }
        if completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the admit/derive turn must complete");
    handle.stop().await.unwrap();

    // The admitted item is back under its original id (identity preserved,
    // no copy) and the derived Note was minted — and neither directive
    // result became a ToolObservation, so the total grew by exactly:
    // user + assistant messages + derived Note = 3. (The admit is a
    // location move from the external map back into the heap — the entry
    // was already part of the logical catalog.)
    let after = engine.diagnostics().await.unwrap();
    let summaries = engine.inspect(100).await.unwrap();
    assert_eq!(
        after.total_items,
        catalog_before + 3,
        "expected user+assistant+derived (admit is a location move), got {} -> {}",
        catalog_before,
        after.total_items
    );
    let admitted = summaries.iter().filter(|s| s.id == target_id).count();
    assert_eq!(
        admitted, 1,
        "the admitted item must exist exactly once under its original id, got {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.kind == ContextKind::Note),
        "the derived fact must be persisted as a new Note"
    );
    assert!(
        engine.inspect_external(target_id).await.unwrap().is_none(),
        "the admitted item must leave the external map (no duplicate owner)"
    );
    // The two seeded step observations remain the only ToolObservations in
    // the logical catalog (one warm in the buffer, one admitted back into
    // the heap); the directives' own results were not persisted.
    let tool_observations = summaries
        .iter()
        .filter(|s| s.kind == ContextKind::ToolObservation)
        .count();
    assert_eq!(
        tool_observations, 2,
        "only the two seeded steps may be ToolObservations, got {summaries:?}"
    );
}

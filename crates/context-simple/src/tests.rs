use agent_contracts::{
    AgentError, AttentionState, ContextAction, ContextEngine, ContextHints, ContextIngress,
    ContextItem, ContextItemId, ContextKind, ContextMaintenanceTrigger, ContextQuery,
    ContextRetention, ContextScope, FocusState, LifecycleLabel, ScopeKind, ScopeState,
    SemanticState, TaskId, ToolOutput,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

/// Open a runtime-owned focus (the engine must never mint a `TaskId`), so
/// the message that follows lands in a real task scope instead of the
/// session fallback.
async fn open_focus(engine: &SimpleContextEngine, goal: &str) -> TaskId {
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, goal),
        })
        .await
        .unwrap();
    task_id
}

#[tokio::test]
async fn successful_observation_is_ephemeral_but_failure_persists_until_verified() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();

    // Round 1: failure — persists (Working) so a later fix can be verified.
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: false,
                summary: "test failed".into(),
                model_content: "error in AuthService.rs:42".into(),
                artifact_ref: Some("artifact://run/test.log".into()),
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.tombstoned_items, 0,
        "a failed observation must persist until verified"
    );

    // Round 2: success on the same entity verifies the fix.
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "2".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "tests passed".into(),
                model_content: "tests passed in AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .any(|t| t.reason.contains("verified fixed")),
        "the error must be archived with a verification reason, got: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );

    // The successful observation itself stays ephemeral and leaves
    // attention after the model turn — consumed, not tombstoned: it stays
    // semantically live and recallable.
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let after = engine.diagnostics().await.unwrap();
    assert!(
        after.archived_items >= 2,
        "the consumed observation and the verified error are both archived"
    );
    assert_eq!(
        after.tombstoned_items, 0,
        "consumption is attention loss, not semantic death"
    );
}

#[tokio::test]
async fn pinned_context_survives_maintenance() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::Pin {
            content: "Never edit generated files".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();

    for _ in 0..20 {
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.contains("Never edit generated files"))
    );
}

#[tokio::test]
async fn maintenance_records_transitions_with_reasons() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "run tests".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "tests ok".into(),
                model_content: "3 passed, 0 failed".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();

    // First maintenance (AfterTool) must not consume the fresh observation
    // (the user message may decay to Cooling; that is normal, not a drop).
    let after_tool = engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    assert!(
        !after_tool
            .transitions
            .iter()
            .any(|t| t.to == AttentionState::Archived && t.reason.contains("observation consumed")),
        "fresh observation must not be consumed at AfterTool: {:?}",
        after_tool.transitions
    );

    // AfterModel with age >= 1 consumes the ephemeral turn observation: it
    // leaves attention (Archived) but stays semantically live and
    // recallable.
    let after_model = engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let consumed = after_model
        .transitions
        .iter()
        .find(|t| t.to == AttentionState::Archived && t.reason.contains("observation consumed"));
    assert!(
        consumed.is_some(),
        "expected a consume transition, got: {:?}",
        after_model.transitions
    );
    let consumed = consumed.unwrap();
    assert_eq!(consumed.kind, ContextKind::ToolObservation);
    assert_eq!(consumed.turn, 1);
    assert!(
        consumed.reason.contains("after model turn"),
        "unexpected reason: {}",
        consumed.reason
    );
    assert_eq!(after_model.turn, 1);
}

#[tokio::test]
async fn checkpoint_restore_roundtrip() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::Pin {
            content: "never touch generated files".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();

    let before = engine.diagnostics().await.unwrap();
    let snapshot_before = engine
        .materialize(ContextQuery {
            current_input: "refactor AuthService".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let consumed_ids: Vec<_> = snapshot_before
        .selected
        .iter()
        .map(|selection| selection.item_id)
        .collect();
    assert!(!consumed_ids.is_empty());

    let checkpoint = engine.checkpoint().await.unwrap();

    let restored = SimpleContextEngine::new(SimpleContextConfig::default());
    restored.restore(checkpoint).await.unwrap();

    let after = restored.diagnostics().await.unwrap();
    assert_eq!(before.total_items, after.total_items);
    assert_eq!(before.turn, after.turn);

    // Access counters survived the round-trip: the same items were consumed.
    let summaries = restored.inspect(usize::MAX).await.unwrap();
    for summary in &summaries {
        if consumed_ids.contains(&summary.id) {
            assert!(
                summary.access_count >= 1,
                "consumed item lost access count: {:?}",
                summary
            );
        }
    }

    // The restored engine remains live.
    restored
        .ingest(ContextIngress::UserMessage {
            content: "continue".into(),
        })
        .await
        .unwrap();
    let grown = restored.diagnostics().await.unwrap();
    assert_eq!(grown.total_items, after.total_items + 1);
}

#[tokio::test]
async fn inspect_is_bounded_and_oldest_first() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    for i in 0..5 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("message {i}"),
            })
            .await
            .unwrap();
    }
    let summaries = engine.inspect(3).await.unwrap();
    assert_eq!(summaries.len(), 3);
    assert_eq!(summaries[0].created_turn, 1);
    assert_eq!(summaries[2].created_turn, 3);
}

#[tokio::test]
async fn completed_task_working_set_is_archived_and_stays_out() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "refactor auth module").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor auth module".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: None,
            summary: "auth refactor done".into(),
        })
        .await
        .unwrap();

    // Archival happens during maintain(TaskCompleted) and is observable.
    let report = engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    let archive = report
        .transitions
        .iter()
        .find(|t| t.to == AttentionState::Archived);
    assert!(
        archive.is_some(),
        "expected an archived transition, got: {:?}",
        report.transitions
    );
    assert!(
        archive.unwrap().reason.contains("task completed"),
        "unexpected reason: {}",
        archive.unwrap().reason
    );

    // A new task must not drag the completed task's details back into the
    // working set: they stay Archived (score below active threshold).
    engine
        .ingest(ContextIngress::UserMessage {
            content: "task two: add tests".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "task two: add tests".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !snapshot
            .items
            .iter()
            .any(|item| item.content.contains("refactor auth module")),
        "completed task details leaked into the new task's working set"
    );
}

#[tokio::test]
async fn later_decision_supersedes_earlier_decision() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use TOML for config".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "switch to YAML instead of TOML".into(),
        })
        .await
        .unwrap();

    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let supersession = report
        .transitions
        .iter()
        .find(|t| t.reason.contains("superseded by decision"));
    assert!(
        supersession.is_some(),
        "the earlier decision must be superseded, got: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );

    // The superseded decision never re-enters the working set (the focus
    // goal may still carry its text — the goal is set once and is the
    // task statement, not the superseded item).
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let working = snapshot
        .items
        .iter()
        .map(|item| item.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !working.contains("use TOML for config"),
        "superseded decision leaked back into the working context"
    );
}

#[tokio::test]
async fn recurring_failure_supersedes_prior_error() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix the build".into(),
        })
        .await
        .unwrap();
    let mut recurrences = 0usize;
    for round in 1..=3 {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: format!("r{round}"),
                    tool_name: "shell.exec".into(),
                    ok: false,
                    summary: format!("round {round} failed"),
                    model_content: "error in Build.kt (module build failed)".into(),
                    artifact_ref: None,
                    metadata: serde_json::Value::Null,
                },
                scope_id: None,
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterTool)
            .await
            .unwrap();
        recurrences += report
            .transitions
            .iter()
            .filter(|t| t.reason.contains("recurring failure supersedes"))
            .count();
    }

    // Two of the three failures were superseded by the next recurrence;
    // exactly one error stays live.
    assert_eq!(recurrences, 2, "two earlier errors superseded");

    let items = engine.inspect(usize::MAX).await.unwrap();
    let live_errors = items
        .iter()
        .filter(|item| {
            item.kind == ContextKind::Error && item.attention != AttentionState::Archived
        })
        .count();
    assert_eq!(
        live_errors, 1,
        "one live error per failure site, got {live_errors}"
    );
}

#[test]
fn baseline_v0_turns_off_every_policy() {
    let v0 = SimpleContextConfig::baseline_v0();
    assert!(!v0.supersession);
    assert!(!v0.error_verification);
    assert!(!v0.entity_affinity);
    assert!(!v0.dependency_expansion);
    // and the defaults keep them on
    let on = SimpleContextConfig::default();
    assert!(on.supersession && on.error_verification);
    assert!(on.entity_affinity && on.dependency_expansion);
}

#[tokio::test]
async fn hot_entities_follow_user_then_tool_then_reset() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());

    // A user message defines the hot set from its own entities.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.hot_entities.contains(&"AuthService.rs".to_string()),
            "user message entities must seed the hot set"
        );
    }

    // A tool observation touching a new file extends it (newest first).
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "CacheStore.rs is hot now".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.hot_entities.first().map(String::as_str),
            Some("CacheStore.rs"),
            "most recently touched entity must lead"
        );
        assert!(state.hot_entities.contains(&"AuthService.rs".to_string()));
    }

    // The next user message resets the hot set.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "unrelated plain words".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.hot_entities.is_empty(),
            "a new user message must reset the hot set"
        );
    }
}

#[tokio::test]
async fn ingest_links_items_sharing_entities() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "tests passed in AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();

    let summaries = engine.inspect(usize::MAX).await.unwrap();
    let user = summaries
        .iter()
        .find(|item| item.kind == ContextKind::UserMessage)
        .expect("user message item");
    let tool = summaries
        .iter()
        .find(|item| item.kind == ContextKind::ToolObservation)
        .expect("tool observation item");

    assert!(
        user.dependencies.is_empty(),
        "first item has nothing to depend on"
    );
    assert!(
        tool.dependencies.contains(&user.id),
        "the tool observation must depend on the prior user message sharing its entity"
    );
}

#[tokio::test]
async fn dependency_expansion_pulls_in_dependencies_within_reserved_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        // A high-value hub and a bulky, low-score dependency of it.
        let hub_id = ContextItemId::new();
        let dep_id = ContextItemId::new();
        let hub = ContextItem {
            id: hub_id,
            task_id: None,
            scope_id: None,
            content: "hub data ".repeat(400), // ~2000 tokens
            kind: ContextKind::UserMessage,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 1.0,
            relevance: 0.5,
            created_tick: 1,
            last_access_tick: 1,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: vec![dep_id],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        let dep = ContextItem {
            id: dep_id,
            task_id: None,
            scope_id: None,
            content: "dependency detail ".repeat(600), // ~10800 chars, ~2700 tokens
            kind: ContextKind::FileObservation,
            scope: ContextScope::Turn,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 2,
            last_access_tick: 2,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        state.items.push(hub);
        state.items.push(dep);
    }

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "go".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    let expansion = snapshot
        .selected
        .iter()
        .find(|selection| selection.reason.contains("included as dependency of item"));
    assert!(
        expansion.is_some(),
        "the low-score dependency must be pulled in by expansion, got: {:?}",
        snapshot
            .selected
            .iter()
            .map(|selection| &selection.reason)
            .collect::<Vec<_>>()
    );
    assert_eq!(snapshot.selected.len(), 2, "hub + its dependency");
    assert!(
        snapshot.approx_tokens <= 4096,
        "expansion must never blow the budget, got {}",
        snapshot.approx_tokens
    );
}

#[tokio::test]
async fn dependency_expansion_can_be_disabled() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let hub_id = ContextItemId::new();
        let dep_id = ContextItemId::new();
        let hub = ContextItem {
            id: hub_id,
            task_id: None,
            scope_id: None,
            content: "hub data ".repeat(400),
            kind: ContextKind::UserMessage,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 1.0,
            relevance: 0.5,
            created_tick: 1,
            last_access_tick: 1,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: vec![dep_id],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        let dep = ContextItem {
            id: dep_id,
            task_id: None,
            scope_id: None,
            content: "dependency detail ".repeat(1200),
            kind: ContextKind::FileObservation,
            scope: ContextScope::Turn,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 2,
            last_access_tick: 2,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        state.items.push(hub);
        state.items.push(dep);
    }

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "go".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    assert!(
        !snapshot
            .selected
            .iter()
            .any(|selection| selection.reason.contains("included as dependency")),
        "with dependency_expansion off the dependency must stay out"
    );
    assert_eq!(snapshot.selected.len(), 1, "only the hub is selected");
}

#[tokio::test]
async fn archived_dependency_below_threshold_stays_out() {
    // Expansion must respect the same active-threshold gate as primary
    // selection: an archived dependency with a cold score is not pulled in.
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        let hub_id = ContextItemId::new();
        let dep_id = ContextItemId::new();
        let hub = ContextItem {
            id: hub_id,
            task_id: None,
            scope_id: None,
            content: "hub data ".repeat(400),
            kind: ContextKind::UserMessage,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 1.0,
            relevance: 0.5,
            created_tick: 1,
            last_access_tick: 1,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: vec![dep_id],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        let dep = ContextItem {
            id: dep_id,
            task_id: None,
            scope_id: None,
            content: "stale dependency".into(),
            kind: ContextKind::FileObservation,
            scope: ContextScope::Turn,
            retention: ContextRetention::Working,
            attention: AttentionState::Archived,
            semantic: SemanticState::Live,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 2,
            last_access_tick: 2,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        state.items.push(hub);
        state.items.push(dep);
    }

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "go".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    assert!(
        !snapshot
            .selected
            .iter()
            .any(|selection| selection.reason.contains("included as dependency")),
        "a cold archived dependency must not be resurrected by expansion"
    );
}

#[tokio::test]
async fn scope_tree_opens_with_session_task_and_focus() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "refactor AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService.rs".into(),
        })
        .await
        .unwrap();

    let diagnostics = engine.diagnostics().await.unwrap();
    assert!(
        diagnostics.active_scopes >= 3,
        "session + task + focus scopes active, got {:?}",
        diagnostics
    );

    let state = engine.state.lock().await;
    assert_eq!(state.scopes.len(), 3);
    let session = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Session)
        .expect("session scope");
    let task = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task)
        .expect("task scope");
    let focus = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Focus)
        .expect("focus scope");
    assert_eq!(task.parent, Some(session.id), "task nests under session");
    assert_eq!(focus.parent, Some(task.id), "focus nests under task");
    assert_eq!(state.active_scope_id, Some(focus.id));
}

#[tokio::test]
async fn task_scope_suspends_when_focus_switches_task() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let first_task = open_focus(&engine, "task one: fix login").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "task one: fix login".into(),
        })
        .await
        .unwrap();
    let second_task = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(second_task, "task two: add tests"),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let first = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(first_task))
        .expect("first task scope");
    assert_eq!(first.state, ScopeState::Suspended);
    let second = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task && scope.task_id != Some(first_task))
        .expect("second task scope");
    assert_eq!(second.state, ScopeState::Active);
    let second_focus = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Focus && scope.parent == Some(second.id))
        .expect("second task focus scope");
    assert_eq!(
        state.active_scope_id,
        Some(second_focus.id),
        "the deepest attention scope is the new task's focus"
    );
    assert!(
        state.scopes.iter().all(|scope| {
            scope.kind != ScopeKind::Focus
                || scope.parent == Some(second.id)
                || scope.state == ScopeState::Suspended
        }),
        "the old task's focus scope suspends with its task"
    );
}

#[tokio::test]
async fn tool_scope_lifecycle_is_runtime_driven() {
    // The tool scope is an execution frame: the runtime opens it at tool
    // start (not at observation ingest) and closes it once the model
    // consumed the result. The observation persisted later carries the
    // scope id, so membership stays authoritative.
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix the build").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix the build".into(),
        })
        .await
        .unwrap();
    let before = engine.diagnostics().await.unwrap();
    assert!(before.active_scopes >= 3, "session + task + focus");

    // Tool start: the runtime opens a fresh tool scope under the focus.
    let tool_scope = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
    {
        let state = engine.state.lock().await;
        let tool = state
            .scopes
            .iter()
            .find(|scope| scope.id == tool_scope)
            .expect("tool scope");
        assert_eq!(tool.kind, ScopeKind::Tool);
        assert_eq!(tool.state, ScopeState::Active);
        assert_eq!(state.active_scope_id, Some(tool_scope));
    }

    // AfterTool does not close the tool scope: the model has not consumed
    // the result yet.
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let tool = state
            .scopes
            .iter()
            .find(|scope| scope.id == tool_scope)
            .expect("tool scope");
        assert_eq!(tool.state, ScopeState::Active);
    }

    // The observation is persisted with the tool scope id even though it
    // happens after the frame's execution.
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "tests pass".into(),
                model_content: "3 passed in Build.kt".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: Some(tool_scope),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let observation = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("tool observation item");
        assert_eq!(
            observation.scope_id,
            Some(tool_scope),
            "the observation is a member of the tool frame by scope_id"
        );
    }

    // The model consumed the result: the runtime closes the tool scope.
    let transitions = engine.close_scope(tool_scope).await.unwrap();
    assert!(
        transitions.is_empty(),
        "successful tool results are ephemeral: nothing to promote or evict"
    );
    {
        let state = engine.state.lock().await;
        let tool = state
            .scopes
            .iter()
            .find(|scope| scope.id == tool_scope)
            .expect("tool scope");
        assert_eq!(tool.state, ScopeState::Closed, "consumed tool scope closes");
        assert!(tool.closed_tick.is_some());
        // The active scope returns to the focus.
        assert_ne!(state.active_scope_id, Some(tool_scope));
    }

    // Closing twice is a no-op.
    let again = engine.close_scope(tool_scope).await.unwrap();
    assert!(again.is_empty());
}

#[tokio::test]
async fn task_close_processes_members_by_scope_id() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "refactor auth module").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor auth module".into(),
        })
        .await
        .unwrap();

    // The user message is a member of the focus scope (created while it was
    // active), and through the tree a member of the task scope as well.
    let state = engine.state.lock().await;
    let task_scope = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task)
        .expect("task scope");
    let user_item = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::UserMessage)
        .expect("user message item");
    assert!(
        user_item.scope_id.is_some(),
        "items must be stamped with their scope at creation"
    );
    let member_scope = user_item.scope_id.unwrap();
    assert_ne!(
        member_scope, task_scope.id,
        "user items belong to the focus"
    );
    let focus = state
        .scopes
        .iter()
        .find(|scope| scope.id == member_scope)
        .expect("focus scope");
    assert_eq!(focus.kind, ScopeKind::Focus);
    drop(state);

    // Completing the task archives the focus-scoped working set through the
    // task scope close.
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: None,
            summary: "auth refactor done".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    let archive = report
        .transitions
        .iter()
        .find(|transition| transition.to == AttentionState::Archived);
    assert!(
        archive.is_some(),
        "task close must archive the focus's working set, got: {:?}",
        report.transitions
    );
    assert!(
        archive.unwrap().reason.contains("task completed"),
        "unexpected reason: {}",
        archive.unwrap().reason
    );
}

#[tokio::test]
async fn task_close_promotes_decisions_and_evicts_the_rest() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    // A decision message (gets the durable "decision" tag) and a plain
    // working message of the same task.
    open_focus(&engine, "use TOML for config").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use TOML for config".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "add a cache note".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: None,
            summary: "config work done".into(),
        })
        .await
        .unwrap();

    let report = engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .any(|t| t.to == AttentionState::Archived && t.reason.contains("task completed")),
        "the working set must be evicted with an explainable reason, got: {:?}",
        report.transitions
    );

    let state = engine.state.lock().await;
    let decision = state
        .items
        .iter()
        .find(|item| item.content.contains("use TOML for config"))
        .expect("decision item");
    assert_eq!(
        decision.scope,
        ContextScope::Session,
        "the decision must be promoted to the session scope"
    );
    assert!(
        decision.semantic.is_live(),
        "a promoted outcome stays semantically live"
    );
    let working = state
        .items
        .iter()
        .find(|item| item.content.contains("cache note"))
        .expect("working item");
    assert_eq!(
        working.attention,
        AttentionState::Archived,
        "the plain working message must be evicted from the closed task"
    );
}

#[tokio::test]
async fn promoted_finding_reactivates_for_a_related_task() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "use TOML for config").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use TOML for config".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: None,
            summary: "config work done".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();

    // The promoted decision keeps its durable markers; while its own entity
    // is still hot it stays active, and a later task touching the same
    // entity keeps it in the working set.
    {
        let state = engine.state.lock().await;
        let decision = state
            .items
            .iter()
            .find(|item| item.content.contains("use TOML for config"))
            .expect("promoted decision");
        assert_eq!(decision.scope, ContextScope::Session);
        assert!(
            decision
                .tags
                .iter()
                .any(|tag| tag.is_lifecycle(agent_contracts::LifecycleLabel::Promoted))
        );
        assert!(
            decision.semantic.is_live(),
            "a promoted outcome stays semantically live"
        );
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: "task two: read the TOML config".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "task two: read the TOML config".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.contains("use TOML for config")),
        "the promoted finding must re-enter the working set for a related task"
    );
}

#[tokio::test]
async fn checkpoint_preserves_scope_tree() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "refactor AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "refactor AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "compiles".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();

    let before = engine.diagnostics().await.unwrap();
    let checkpoint = engine.checkpoint().await.unwrap();

    let restored = SimpleContextEngine::new(SimpleContextConfig::default());
    restored.restore(checkpoint).await.unwrap();
    let after = restored.diagnostics().await.unwrap();
    assert_eq!(before.total_items, after.total_items);
    assert_eq!(before.active_scopes, after.active_scopes);
    assert_eq!(before.closed_scopes, after.closed_scopes);

    // The restored scope tree keeps working: another turn extends the same
    // task and focus scopes instead of duplicating them.
    restored
        .ingest(ContextIngress::UserMessage {
            content: "continue".into(),
        })
        .await
        .unwrap();
    let grown = restored.diagnostics().await.unwrap();
    assert_eq!(grown.active_scopes, before.active_scopes);
    {
        let state = restored.state.lock().await;
        assert_eq!(
            state
                .scopes
                .iter()
                .filter(|scope| scope.kind == ScopeKind::Task)
                .count(),
            1,
            "one task scope, reused after restore"
        );
    }
}

#[tokio::test]
async fn max_selected_items_hint_caps_the_working_set() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    for i in 0..6 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("request {i}"),
            })
            .await
            .unwrap();
    }

    let uncapped = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        uncapped.items.len() > 3,
        "without the hint the budget decides, got {}",
        uncapped.items.len()
    );

    let capped = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 8192,
            hints: ContextHints {
                max_selected_items: Some(2),
            },
        })
        .await
        .unwrap();
    assert_eq!(
        capped.items.len(),
        2,
        "the hint must cap the working set, got {}",
        capped.items.len()
    );
    assert_eq!(capped.selected.len(), 2);
}

#[tokio::test]
async fn pinned_items_get_priority_but_never_break_the_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        let small_pin = ContextItem {
            id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            content: "small pin ".repeat(10), // ~25 tokens
            kind: ContextKind::Constraint,
            scope: ContextScope::Pinned,
            retention: ContextRetention::Pinned,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 1.0,
            relevance: 1.0,
            created_tick: 1,
            last_access_tick: 1,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some("pinned".into()),
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        let oversized_pin = ContextItem {
            id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            content: "huge pinned data ".repeat(5_000), // ~7500 tokens
            kind: ContextKind::Constraint,
            scope: ContextScope::Pinned,
            retention: ContextRetention::Pinned,
            attention: AttentionState::Active,
            semantic: SemanticState::Live,
            importance: 1.0,
            relevance: 1.0,
            created_tick: 2,
            last_access_tick: 2,
            access_count: 0,
            created_turn: 1,
            last_access_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some("pinned".into()),
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
        };
        state.items.push(small_pin);
        state.items.push(oversized_pin);
    }

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    assert!(
        snapshot.approx_tokens <= 4096,
        "the budget is a hard bound even for pinned items, got {}",
        snapshot.approx_tokens
    );
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.content.starts_with("small pin")),
        "the small pin must be selected first"
    );
    assert!(
        !snapshot
            .items
            .iter()
            .any(|item| item.content.starts_with("huge pinned data")),
        "an oversized pinned item must not blow the budget"
    );
}

#[tokio::test]
async fn task_close_promotes_archived_durable_outcomes_and_resyncs_scope_id() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_id = open_focus(&engine, "use AuthService.rs as the auth layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs as the auth layer".into(),
        })
        .await
        .unwrap();
    // A working observation in the same task: ephemeral, must not promote.
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "touched AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();

    // The durable decision cooled to Archived while the task ran. The close
    // must still promote it: Archived is an attention state, not semantic
    // death.
    let decision_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("decision item")
            .id
    };
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.id == decision_id {
                item.attention = AttentionState::Archived;
                item.relevance = 0.0;
                // A high-value durable outcome; the residency pass after the
                // close must not immediately demote it again.
                item.importance = 1.0;
            }
        }
    }

    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_id),
            summary: "auth layer decided".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    assert!(
        report.transitions.iter().any(|t| {
            t.item_id == decision_id
                && t.from == AttentionState::Archived
                && t.to == AttentionState::Active
        }),
        "the archived durable outcome must promote on close, got: {:?}",
        report.transitions
    );

    let state = engine.state.lock().await;
    let decision = state
        .items
        .iter()
        .find(|item| item.id == decision_id)
        .expect("decision item");
    assert_eq!(
        decision.attention,
        AttentionState::Active,
        "promoted outcomes return to the active set"
    );
    assert_eq!(
        decision.scope,
        ContextScope::Session,
        "promoted to the nearest open ancestor"
    );
    let session = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Session)
        .expect("session scope");
    assert_eq!(
        decision.scope_id,
        Some(session.id),
        "the authoritative scope_id membership must follow the promotion"
    );
    assert!(
        decision
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
        "the promotion is labeled and explainable"
    );
    let task_scope = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task)
        .expect("task scope");
    assert_eq!(task_scope.state, ScopeState::Closed);
}

// ---------------------------------------------------------------------------
// Model/operator context directives: `ContextIngress::ContextDirective`
// applies gc hints, tags and leases; GC treats the targeted items as roots
// until the hint is cleared or the lease expires. Every protection is
// explainable in the eviction/reactivation reasons.
// ---------------------------------------------------------------------------

fn observation_output(id: &str, ok: bool, content: &str) -> ToolOutput {
    ToolOutput {
        call_id: id.into(),
        tool_name: "shell.exec".into(),
        ok,
        summary: "ok".into(),
        model_content: content.into(),
        artifact_ref: None,
        metadata: serde_json::json!({}),
    }
}

/// A consumed observation (Archived + Ephemeral + Turn) outside the focus
/// scope chain: the default GC heuristic evicts it, so a directive's
/// protection is the only thing keeping it resident.
async fn consumed_observation_outside_focus(engine: &SimpleContextEngine) -> ContextItemId {
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: observation_output("1", true, "tests passed in AuthService.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let items = engine.inspect(usize::MAX).await.unwrap();
    let observation_id = items
        .iter()
        .find(|item| item.kind == ContextKind::ToolObservation)
        .expect("the observation exists")
        .id;
    {
        let mut state = engine.state.lock().await;
        for item in &mut state.items {
            if item.id == observation_id {
                item.scope_id = None; // outside the focus scope chain
                item.content = "fix CacheStore.rs".into();
                item.entities = crate::index::entity::extract_entities(&item.content);
            }
        }
    }
    observation_id
}

#[tokio::test]
async fn gc_hint_keeps_a_consumed_observation_resident_until_cleared() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let observation_id = consumed_observation_outside_focus(&engine).await;

    // A keep_alive hint protects the consumed observation: the model asked
    // for the item to stay, so it is a GC root despite being consumed.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: observation_id,
                keep_alive: true,
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.evicted, 0,
        "the hinted item must be a GC root: {report:?}"
    );

    // Clearing the hint releases it again.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: observation_id,
                keep_alive: false,
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.evicted >= 1,
        "a released item is evictable again: {report:?}"
    );
}

#[tokio::test]
async fn hint_on_an_evicted_item_brings_it_back_on_the_next_gc() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let observation_id = consumed_observation_outside_focus(&engine).await;

    // The item is evicted by the first GC pass...
    let report = engine.gc().await.unwrap();
    assert!(report.evicted >= 1, "baseline: consumed observation evicts");
    let warm = engine.diagnostics().await.unwrap().warm_items;
    assert_eq!(warm, 1, "the observation sits in the reversible buffer");

    // ...and a hint applied afterwards reactivates it: directives reach
    // buffer items, and GC treats the hint as a root claim.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: observation_id,
                keep_alive: true,
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.reactivated, 1,
        "the hinted buffer item must come back: {report:?}"
    );
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("kept alive")),
        "the reactivation must be explainable: {:?}",
        report.reactivations
    );
}

#[tokio::test]
async fn lease_protects_an_item_until_it_expires() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let observation_id = consumed_observation_outside_focus(&engine).await;
    // state.turn == 1 here (one user message so far).
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: observation_id,
                turns: 1,
            },
        })
        .await
        .unwrap();

    // Protected at lease time and one turn later (inclusive until_turn).
    let report = engine.gc().await.unwrap();
    assert_eq!(report.evicted, 0, "leased until the next turn: {report:?}");
    engine
        .ingest(ContextIngress::UserMessage {
            content: "continue working".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(report.evicted, 0, "lease covers turn 2 too: {report:?}");

    // One turn after the lease ran out, the item is evictable again.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "next task".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.evicted >= 1,
        "an expired lease no longer protects: {report:?}"
    );
}

#[tokio::test]
async fn tag_directive_attaches_an_extension_label_to_the_target() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let observation_id = consumed_observation_outside_focus(&engine).await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Tag {
                item_id: observation_id,
                tag: "urgent".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let item = state
        .items
        .iter()
        .find(|item| item.id == observation_id)
        .expect("the tagged item exists");
    assert!(
        item.tags.iter().any(|tag| tag.as_str() == "ext:urgent"),
        "the extension tag must be attached: {:?}",
        item.tags
    );
}

#[tokio::test]
async fn directive_with_unknown_item_id_is_a_silent_noop() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    let before = engine.diagnostics().await.unwrap().total_items;

    for action in [
        ContextAction::GcHint {
            item_id: ContextItemId::new(),
            keep_alive: true,
        },
        ContextAction::Tag {
            item_id: ContextItemId::new(),
            tag: "gone".into(),
        },
        ContextAction::Lease {
            item_id: ContextItemId::new(),
            turns: 3,
        },
    ] {
        engine
            .ingest(ContextIngress::ContextDirective { action })
            .await
            .unwrap();
    }
    let after = engine.diagnostics().await.unwrap();
    assert_eq!(
        after.total_items, before,
        "a stale directive must not create or destroy items"
    );
}

/// Open a focus and produce `n` tool observations inside its task scope.
async fn observations_in_focus(engine: &SimpleContextEngine, n: usize) -> Vec<ContextItemId> {
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on the service layer".into(),
        })
        .await
        .unwrap();
    for i in 0..n {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: observation_output(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i} completed"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let items = engine.inspect(usize::MAX).await.unwrap();
    items
        .iter()
        .filter(|item| item.kind == ContextKind::ToolObservation)
        .map(|item| item.id)
        .collect()
}

#[tokio::test]
async fn keep_alive_quota_refuses_extra_hints_until_one_is_released() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_keep_alive_items: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    let ids = observations_in_focus(&engine, 2).await;

    let hint = |item_id, keep_alive| ContextIngress::ContextDirective {
        action: ContextAction::GcHint {
            item_id,
            keep_alive,
        },
    };

    // The first hint fits the quota...
    engine.ingest(hint(ids[0], true)).await.unwrap();
    // ...the second is refused and the reason is surfaced to the model.
    let err = engine.ingest(hint(ids[1], true)).await.unwrap_err();
    match &err {
        AgentError::InvalidRequest(reason) => {
            assert!(
                reason.contains("keep_alive"),
                "the refusal must explain the quota: {reason}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }

    // Releasing an item frees the slot: the same hint now applies.
    engine.ingest(hint(ids[0], false)).await.unwrap();
    engine.ingest(hint(ids[1], true)).await.unwrap();
    {
        let state = engine.state.lock().await;
        let kept = state
            .items
            .iter()
            .filter(|item| item.keep_alive)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(kept, vec![ids[1]], "only the hinted item stays keep_alive");
    }
}

#[tokio::test]
async fn lease_turns_are_clamped_to_the_config_cap() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_lease_turns: 4,
        ..SimpleContextConfig::default()
    });
    let task_id = open_focus(&engine, "service layer").await;
    let ids = observations_in_focus(&engine, 1).await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: ids[0],
                turns: 1000,
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .expect("the observation exists");
        // One user message was ingested, so state.turn == 1 here; the lease
        // is clamped to the cap instead of running "forever".
        assert_eq!(
            item.lease_until_turn,
            Some(state.turn.saturating_add(4)),
            "the lease must be clamped to max_lease_turns"
        );
        assert_eq!(item.task_id, Some(task_id));
    }
}

#[tokio::test]
async fn lease_count_quota_is_per_task_and_renewal_is_free() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_leased_items_per_task: 1,
        ..SimpleContextConfig::default()
    });
    let task_a = open_focus(&engine, "task A").await;
    let ids = observations_in_focus(&engine, 2).await;

    let lease = |item_id| ContextIngress::ContextDirective {
        action: ContextAction::Lease { item_id, turns: 2 },
    };

    // The first item in the task leases fine, and renewing it adds no new
    // protected item, so the renewal stays allowed...
    engine.ingest(lease(ids[0])).await.unwrap();
    engine.ingest(lease(ids[0])).await.unwrap();
    // ...a second distinct item in the same task is refused.
    let err = engine.ingest(lease(ids[1])).await.unwrap_err();
    match &err {
        AgentError::InvalidRequest(reason) => {
            assert!(
                reason.contains("items (cap 1)"),
                "the refusal must name the count quota: {reason}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }

    // A different task owns its own budget: the cap does not leak across
    // tasks, and task A keeps exactly its one lease.
    open_focus(&engine, "task B").await;
    let other = observations_in_focus(&engine, 1).await;
    engine.ingest(lease(other[0])).await.unwrap();
    {
        let state = engine.state.lock().await;
        let leased_in_a = state
            .items
            .iter()
            .filter(|item| item.task_id == Some(task_a) && item.lease_until_turn.is_some())
            .count();
        assert_eq!(leased_in_a, 1, "task A keeps exactly its one lease");
    }
}

#[tokio::test]
async fn lease_token_quota_bounds_the_weight_of_protected_items() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_leased_items_per_task: 8,
        max_leased_tokens_per_task: 100,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "big task").await;
    let big = "x".repeat(300); // ~75 tokens
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on the service layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: observation_output("big-1", true, &big),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: observation_output("big-2", true, &big),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let items = engine.inspect(usize::MAX).await.unwrap();
    let ids: Vec<_> = items
        .iter()
        .filter(|item| item.kind == ContextKind::ToolObservation)
        .map(|item| item.id)
        .collect();
    assert_eq!(ids.len(), 2);

    let lease = |item_id| ContextIngress::ContextDirective {
        action: ContextAction::Lease { item_id, turns: 2 },
    };
    // One ~75-token item fits a 100-token budget; the second does not.
    engine.ingest(lease(ids[0])).await.unwrap();
    let err = engine.ingest(lease(ids[1])).await.unwrap_err();
    match &err {
        AgentError::InvalidRequest(reason) => {
            assert!(
                reason.contains("tokens (cap 100)"),
                "the refusal must name the token quota: {reason}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn task_close_expires_keep_alive_and_leases() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_id = open_focus(&engine, "service layer").await;
    let ids = observations_in_focus(&engine, 1).await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: ids[0],
                keep_alive: true,
            },
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: ids[0],
                turns: 100,
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .expect("the observation exists");
        assert!(
            item.keep_alive && item.lease_until_turn.is_some(),
            "the protections are active while the task runs"
        );
    }

    // Completing the task clears the model protections: a finished task
    // cannot keep rooting its working set forever.
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_id),
            summary: "service layer done".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.id == ids[0])
            .expect("the observation exists");
        assert!(!item.keep_alive, "keep_alive expires with the task");
        assert_eq!(item.lease_until_turn, None, "leases expire with the task");
    }

    // Freed from protection, the consumed observation is evictable again.
    let report = engine.gc().await.unwrap();
    assert!(
        report.evicted >= 1,
        "the completed task's working set is evictable: {report:?}"
    );
}

#[tokio::test]
async fn pinned_dependency_cannot_break_the_expansion_budget() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());

    // A huge pinned constraint: it shares an entity with the observation
    // below, so the observation links to it as a dependency.
    let huge_pin = format!("AuthService.rs pinned constraint {}", "x".repeat(8000));
    engine
        .ingest(ContextIngress::Pin {
            content: huge_pin,
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "AuthService.rs tests passed".into(),
                artifact_ref: None,
                metadata: serde_json::json!({}),
            },
            scope_id: None,
        })
        .await
        .unwrap();

    // Budget big enough for the small observation (primary pass) but far
    // below the pin's token cost: the pin must be reachable only through
    // dependency expansion, where it no longer gets a pinned exemption.
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 1100,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();

    let ids: Vec<ContextItemId> = snapshot.items.iter().map(|item| item.item_id).collect();
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.kind == ContextKind::ToolObservation),
        "the small observation must be selected"
    );
    let pin_in_frame = snapshot
        .items
        .iter()
        .any(|item| item.retention == ContextRetention::Pinned);
    assert!(
        !pin_in_frame,
        "an oversized pinned dependency must not break the expansion budget"
    );
    assert!(
        snapshot.approx_tokens <= 1100 + 512,
        "the frame must stay near the budget (got {})",
        snapshot.approx_tokens
    );
    let _ = ids;
}

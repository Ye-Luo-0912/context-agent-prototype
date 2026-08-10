use std::sync::Arc;

use agent_contracts::{
    AgentError, AttentionState, ContextAction, ContextConsumptionAck, ContextEngine, ContextHints,
    ContextIngress, ContextItem, ContextItemId, ContextKind, ContextMaintenanceTrigger,
    ContextQuery, ContextResidency, ContextRetention, ContextScope, ContextSearchQuery, CoreLabel,
    DependencyEdge, DependencyKind, FocusState, Label, LifecycleLabel, MaterializedContext,
    OperationId, ScopeId, ScopeKind, ScopeState, SemanticState, TaskId, ToolOutput, TurnId,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

async fn acknowledge_all(engine: &SimpleContextEngine, materialized: &MaterializedContext) {
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: materialized.materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
        })
        .await
        .unwrap();
}

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
    acknowledge_all(&engine, &snapshot_before).await;

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
async fn materialize_is_preview_and_ack_reinforces_only_the_final_subset() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    for content in ["keep constraint alpha", "keep constraint beta"] {
        engine
            .ingest(ContextIngress::Pin {
                content: content.into(),
                kind: ContextKind::Constraint,
            })
            .await
            .unwrap();
    }
    let preview = engine
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(preview.items.len(), 2);
    assert!(
        engine
            .inspect(usize::MAX)
            .await
            .unwrap()
            .iter()
            .all(|item| item.access_count == 0),
        "previewing candidates must not pretend the model consumed them"
    );

    let kept = preview.items[0].item_id;
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: preview.materialization_id,
            item_ids: vec![kept],
            external_item_ids: Vec::new(),
        })
        .await
        .unwrap();
    let summaries = engine.inspect(usize::MAX).await.unwrap();
    assert_eq!(
        summaries
            .iter()
            .find(|item| item.id == kept)
            .unwrap()
            .access_count,
        1
    );
    assert!(
        summaries
            .iter()
            .filter(|item| item.id != kept)
            .all(|item| item.access_count == 0),
        "an actor-trimmed item must receive no reinforcement"
    );
}

#[tokio::test]
async fn invalid_consumption_ack_is_atomic_and_the_exact_retry_can_commit() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::Pin {
            content: "retain exact evidence".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    let preview = engine
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let real_id = preview.items[0].item_id;
    let invalid = ContextConsumptionAck {
        turn_id: TurnId::new(),
        operation_id: OperationId::new(),
        model_round: 1,
        materialization_id: preview.materialization_id,
        item_ids: vec![real_id, ContextItemId::new()],
        external_item_ids: Vec::new(),
    };
    assert!(engine.acknowledge_consumption(invalid).await.is_err());
    assert_eq!(engine.inspect(usize::MAX).await.unwrap()[0].access_count, 0);

    acknowledge_all(&engine, &preview).await;
    assert_eq!(engine.inspect(usize::MAX).await.unwrap()[0].access_count, 1);
}

#[tokio::test]
async fn consumption_ack_rejects_cross_residency_duplicate_ownership() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::Pin {
            content: "single-owner evidence".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    let preview = engine
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        let duplicate = state.items.iter().next().unwrap().clone();
        state.eviction_buffer.push(duplicate);
    }

    let error = engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: preview.materialization_id,
            item_ids: preview.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exactly one residency owner"));
    assert_eq!(engine.inspect(usize::MAX).await.unwrap()[0].access_count, 0);
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
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
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
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
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
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
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
async fn external_retrieval_searches_inspects_and_fetches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_a = TaskId::new();
    let task_b = TaskId::new();
    let (item_a_id, item_b_id) = {
        let mut state = engine.state.lock().await;
        let mut item_a = crate::item::make_item(
            &state,
            &engine.config,
            "AuthService.rs decision: replace the auth flow".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.5,
            None,
        );
        item_a.id = ContextItemId::new();
        item_a.task_id = Some(task_a);
        item_a.entities = crate::index::entity::extract_entities(&item_a.content);
        item_a.tags.push(Label::core(CoreLabel::Decision));
        let mut item_b = crate::item::make_item(
            &state,
            &engine.config,
            "CacheStore.rs note: LRU eviction order".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        item_b.id = ContextItemId::new();
        item_b.task_id = Some(task_b);
        item_b.entities = crate::index::entity::extract_entities(&item_b.content);
        for (item, tick) in [(&item_a, 1u64), (&item_b, 2u64)] {
            let reference = crate::store::externalize(dir.path(), item).unwrap();
            state.external.push(crate::store::to_external_entry(
                item, reference, tick, 1, None,
            ));
        }
        (item_a.id, item_b.id)
    };

    // Search by entity signature: only the decision matches.
    let hits = engine
        .search_external(ContextSearchQuery::new("AuthService", 10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "entity search must hit exactly one ref");
    assert_eq!(hits[0].item_id, item_a_id);
    assert_eq!(hits[0].task_id, Some(task_a));

    // Kind filter without a query.
    let hits = engine
        .search_external(ContextSearchQuery {
            query: String::new(),
            kind: Some(ContextKind::Note),
            scope: None,
            task_id: None,
            limit: 0,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, item_b_id);

    // Task filter: task A owns exactly its own ref.
    let hits = engine
        .search_external(ContextSearchQuery {
            query: String::new(),
            kind: None,
            scope: None,
            task_id: Some(task_a),
            limit: 0,
        })
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item_id, item_a_id);

    // Inspect: metadata without a store read.
    let inspected = engine
        .inspect_external(item_a_id)
        .await
        .unwrap()
        .expect("entry exists");
    assert_eq!(inspected.kind, ContextKind::Decision);
    assert!(
        inspected
            .tags
            .iter()
            .any(|t| t.is_core(CoreLabel::Decision))
    );

    // Fetch: full content back, and the access is stamped on the entry so
    // recency ranking and Cold -> External aging stay honest.
    let fetched = engine
        .fetch_external(item_a_id)
        .await
        .unwrap()
        .expect("item readable back");
    assert!(fetched.content.contains("replace the auth flow"));
    {
        let state = engine.state.lock().await;
        let entry = state
            .external
            .iter()
            .find(|e| e.item_id == item_a_id)
            .expect("entry survives the fetch");
        assert_eq!(entry.last_access_tick, state.tick);
        assert_eq!(entry.last_access_gc_epoch, Some(state.gc_epoch));
    }

    // The item stays externalized: fetch is a read, not a reactivation.
    let items = engine.inspect(usize::MAX).await.unwrap();
    assert!(
        items.iter().all(|item| item.id != item_a_id),
        "fetch must not re-enter the working set"
    );
}

#[tokio::test]
async fn terminal_external_entries_are_hidden_from_every_retrieval_surface() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });

    let (live_id, terminal_ids) = {
        let mut state = engine.state.lock().await;
        let semantics = [
            SemanticState::Live,
            SemanticState::Superseded { by: None },
            SemanticState::VerifiedFixed { by: None },
            SemanticState::Tombstoned,
        ];
        let mut ids = Vec::new();
        for (offset, semantic) in semantics.into_iter().enumerate() {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("RecallSurface.rs entry {offset}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                None,
            );
            item.semantic = semantic;
            item.entities = crate::index::entity::extract_entities(&item.content);
            let reference = crate::store::externalize(dir.path(), &item).unwrap();
            let mut entry =
                crate::store::to_external_entry(&item, reference, offset as u64 + 1, 1, None);
            if matches!(semantic, SemanticState::VerifiedFixed { .. }) {
                entry.residency = agent_contracts::ContextResidency::External;
            }
            state.external.push(entry);
            ids.push(item.id);
        }
        (ids[0], ids[1..].to_vec())
    };

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "RecallSurface.rs".into(),
            budget_tokens: 1_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        materialized
            .external
            .iter()
            .map(|entry| entry.item_id)
            .collect::<Vec<_>>(),
        vec![live_id],
        "the materialized external view must expose only live refs"
    );

    let hits = engine
        .search_external(ContextSearchQuery::new("RecallSurface", 10))
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|entry| entry.item_id).collect::<Vec<_>>(),
        vec![live_id],
        "search must not return semantically terminal refs"
    );

    for item_id in terminal_ids {
        assert!(
            engine.inspect_external(item_id).await.unwrap().is_none(),
            "inspect must hide terminal ref {item_id:?}"
        );
        assert!(
            engine.fetch_external(item_id).await.unwrap().is_none(),
            "fetch must refuse terminal ref {item_id:?}"
        );
    }
}

#[tokio::test]
async fn consumption_ack_stamps_an_external_descriptor_without_reactivating_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "ExternalAck.rs historical decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.6,
            None,
        );
        item.entities = crate::index::entity::extract_entities(&item.content);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 0, 0, None,
        ));
        item.id
    };

    let preview = engine
        .materialize(ContextQuery {
            current_input: "inspect ExternalAck.rs".into(),
            budget_tokens: 2_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        preview
            .external
            .iter()
            .map(|entry| entry.item_id)
            .collect::<Vec<_>>(),
        vec![item_id]
    );
    let before = engine.inspect_external(item_id).await.unwrap().unwrap();
    assert_eq!(before.last_access_tick, 0, "preview must not stamp access");

    acknowledge_all(&engine, &preview).await;

    let after = engine.inspect_external(item_id).await.unwrap().unwrap();
    assert!(after.last_access_tick > before.last_access_tick);
    assert_eq!(after.last_access_gc_epoch, Some(0));
    assert!(
        engine
            .inspect(usize::MAX)
            .await
            .unwrap()
            .iter()
            .all(|item| item.id != item_id),
        "acknowledging a descriptor must not page its body back into memory"
    );
}

#[tokio::test]
async fn gc_externalizes_overflow_and_recalls_via_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for i in 0..3 {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: observation_output(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i}: fix AuthService.rs"),
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

    // The buffer is capped at 1, so two of the three consumed observations
    // overflow into the store — without the lock being held during the
    // writes (the plan/commit split keeps disk IO outside the state lock).
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.externalized, 2,
        "buffer overflow must externalize to the store: {report:?}"
    );
    let stored = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(stored, report.externalized, "the store files must exist");

    // Hot entities belonging to an externalized item recall it: the store
    // read happens in the IO phase and the item re-enters the heap.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "continue on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivated >= 1,
        "a hot externalized item must be recalled: {report:?}"
    );
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("recalled from the context store")),
        "the recall must be explainable: {:?}",
        report.reactivations
    );
    // Recalled content is resident again, so its blobs are deleted only
    // *after* the commit landed — every formal blob has exactly one owner,
    // and a crash between commit and delete leaves an orphan the startup
    // reconcile re-owns. One owner, one file.
    let stored = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(
        stored, 0,
        "recalled blobs must be removed once their content is resident"
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

#[tokio::test]
async fn closed_tool_scopes_are_not_candidates_but_hot_entities_still_reach_them() {
    // A low active threshold so the assertion targets *candidacy* (scope
    // membership) rather than the scoring cutoff.
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        active_threshold: 0.1,
        ..SimpleContextConfig::default()
    });
    let _task_id = open_focus(&engine, "refactor AuthService.rs").await;
    let task_id = _task_id;

    // A tool frame opens, an observation lands in it, the frame closes.
    // The observation keeps its scope stamp — it is not promoted or
    // evicted by the tool close, it just loses its candidacy.
    let tool_scope = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
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
            scope_id: Some(tool_scope),
        })
        .await
        .unwrap();
    engine.close_scope(tool_scope).await.unwrap();

    // Without hot entities the closed frame's observation is not a
    // candidate: it is no longer part of the open working-set lineage,
    // even though it still belongs to the active task. The observation
    // itself seeded the hot set at ingest, so re-focus with an empty hot
    // set first.
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "refactor AuthService.rs"),
        })
        .await
        .unwrap();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 10_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !materialized
            .items
            .iter()
            .any(|item| item.content.contains("touched")),
        "a closed tool scope's observation must not be a candidate: {:?}",
        materialized
            .items
            .iter()
            .map(|item| &item.content)
            .collect::<Vec<_>>()
    );

    // When the entity is hot again the same item becomes a candidate
    // again — closed scope membership is not a one-way door; affinity
    // recalls it, exactly like the GC's hot-entity reactivation.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "recall what we changed in AuthService.rs".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let observation = state
            .items
            .iter()
            .find(|item| item.content.contains("touched"))
            .expect("the observation is still in the heap");
        assert!(
            state.hot_entities.iter().any(|e| e == "AuthService.rs"),
            "hot entities after the user message: {:?}",
            state.hot_entities
        );
        assert!(
            observation.entities.iter().any(|e| e == "AuthService.rs"),
            "observation entities: {:?}",
            observation.entities
        );
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "recall what we changed in AuthService.rs".into(),
            budget_tokens: 10_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.content.contains("touched")),
        "hot entities must recall the closed frame's observation"
    );
}

/// The external store is a fidelity boundary: `fetch(ref)` must recover the
/// exact content that was externalized, not a summary or a truncated copy.
#[tokio::test]
async fn fetch_external_recovers_the_exact_original_content() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let contents = ["step 0: fix AuthService.rs", "step 1: fix AuthService.rs"];
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
        "buffer overflow must externalize: {report:?}"
    );

    // Find one externalized ref through the retrieval surface, then pull
    // its full content back across the store boundary.
    let refs = engine
        .search_external(agent_contracts::ContextSearchQuery {
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
        "the search must surface the externalized refs"
    );
    let fetched = engine
        .fetch_external(refs[0].item_id)
        .await
        .unwrap()
        .expect("fetch must return the externalized item");
    assert_eq!(
        fetched.id, refs[0].item_id,
        "fetch must return the item the ref points at"
    );
    assert_eq!(
        fetched.kind,
        ContextKind::ToolObservation,
        "the recovered item keeps its kind"
    );
    assert!(
        contents.contains(&fetched.content.as_str()),
        "fetch must recover the exact original content, got: {:?}",
        fetched.content
    );
}

/// The context store is confined: with an explicit store dir every write
/// lands under it, and the default fallback is an OS temp dir — never a
/// CWD-relative path, so a misconfigured runtime cannot scatter externalized
/// content into the launch directory.
#[tokio::test]
async fn context_store_never_writes_outside_the_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("context-store");
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(store.clone()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for i in 0..3 {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: observation_output(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i}: fix AuthService.rs"),
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
    let report = engine.gc().await.unwrap();
    assert!(report.externalized >= 1, "overflow must externalize");

    // Every store file is inside the configured directory and nothing else
    // was created nearby.
    let files: Vec<_> = std::fs::read_dir(&store).unwrap().collect();
    assert_eq!(
        files.len(),
        report.externalized,
        "all store files land in the configured dir"
    );
    for file in files {
        let path = file.unwrap().path();
        assert!(path.starts_with(&store), "store file escaped: {path:?}");
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some("json"),
            "store files are the item payloads"
        );
    }
    // The old CWD-relative fallback must never appear.
    let legacy = std::env::current_dir()
        .unwrap()
        .join(".focus-agent")
        .join("context-store");
    assert!(
        !legacy.exists(),
        "no store may be created under the CWD: {}",
        legacy.display()
    );

    // The default fallback is an OS temp dir, never CWD-derived.
    let default_dir = crate::store::store_dir(&SimpleContextConfig::default());
    assert!(
        default_dir.starts_with(std::env::temp_dir()),
        "the default store must live under the OS temp dir, got: {default_dir:?}"
    );
    assert!(
        !default_dir.starts_with(std::env::current_dir().unwrap()),
        "the default store must never be CWD-relative: {default_dir:?}"
    );
}

/// Build a successful tool observation for a turn.
async fn tool_observation(engine: &SimpleContextEngine, call_id: &str, content: &str) {
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: content.into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
}

/// Build a failing tool observation for a turn (persists as an Error).
async fn failed_observation(engine: &SimpleContextEngine, call_id: &str, content: &str) {
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "shell.exec".into(),
                ok: false,
                summary: "failed".into(),
                model_content: content.into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
}

/// Long-task acceptance (`long_task_10k_turns`): over 10,000 task turns the
/// resident working set is bounded by the current episode plus unresolved
/// semantic state, not by turn count. Required decisions stay recallable;
/// stale ordinary dialogue leaves Resident.
#[tokio::test]
async fn long_task_10k_turns_keeps_the_working_set_episode_bounded() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Test-only semantic boundary: consecutive per-turn messages from
        // different workstreams share almost no tokens, so the episode
        // rotates on the semantic signal (the default threshold is
        // deliberately more conservative).
        episode_rotate_threshold: 0.35,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "maintain the auth service").await;

    // Turn 0: a durable decision the task must keep recalling.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "0", "reviewed AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    // Most turns come from different workstreams (semantic rotation); the
    // final burst is a stream of related messages (turn-budget rotation).
    const WORKSTREAMS: &[&str] = &[
        "fix the auth cache invalidation",
        "refactor retry backoff for shards",
        "add request tracing to the gateway",
        "tune connection pool sizing",
        "investigate the token bucket throttle",
        "rework circuit breaker thresholds",
        "profile event bus dispatch latency",
        "harden the input validation path",
        "reduce index rebuild cost",
        "document the deployment runbook",
    ];
    let mut max_resident = 0usize;
    let mut early_ordinary_id = None;
    let mut resident_at_2000 = 0usize;
    for turn in 1..=10_000u64 {
        let content = if turn <= 9_000 {
            format!(
                "{} in round {}",
                WORKSTREAMS[turn as usize % WORKSTREAMS.len()],
                turn
            )
        } else {
            // Related messages: the semantic signal never fires, so the
            // episode must rotate on the 500-turn budget instead.
            format!("keep working on the auth cache and the retry backoff in round {turn}")
        };
        engine
            .ingest(ContextIngress::UserMessage { content })
            .await
            .unwrap();
        tool_observation(
            &engine,
            &turn.to_string(),
            &format!("patched Item{}", turn % 13),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if turn % 50 == 0 {
            engine.gc().await.unwrap();
            let resident = engine.state.lock().await.items.len();
            max_resident = max_resident.max(resident);
            if turn == 2_000 {
                resident_at_2000 = resident;
            }
        }
        // Record the turn-100 ordinary message while it is still resident
        // (it is evicted by the very next episode rotation).
        if turn == 100 {
            early_ordinary_id = engine
                .state
                .lock()
                .await
                .items
                .iter()
                .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 100)
                .map(|item| item.id);
        }
    }

    // 1. Bounded working set: without episode rotation 10,000 turns would
    // leave ~20,000 resident items; rotation keeps the peak to the current
    // episode plus hot recalls (the 500-turn budget burst is dominated by
    // GC and never accumulates).
    assert!(
        max_resident < 200,
        "resident working set must stay bounded, peak was {max_resident}"
    );
    // 2. Bounded *over time*: the working set must not grow with turn
    // count. A linear-growth engine would show a large delta between turn
    // 2,000 and turn 10,000.
    let resident_at_10000 = engine.state.lock().await.items.len();
    assert!(
        resident_at_10000 <= resident_at_2000.saturating_add(20),
        "the working set must not grow with turn count: {resident_at_2000} -> {resident_at_10000}"
    );

    // 2. Stale ordinary dialogue leaves Resident.
    let early = early_ordinary_id.expect("an early ordinary message id");
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|item| item.id == early),
            "stale ordinary dialogue must leave the resident heap"
        );
    }

    // 3. The required decision stays recallable: touch its entity, then
    // materialize and expect it back in the working set.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we decide about AuthService.rs?".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "final", "touched AuthService.rs again").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "what did we decide about AuthService.rs?".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.kind == ContextKind::UserMessage
                && item.content.contains("AuthService.rs")),
        "the required decision must stay recallable, selected: {:?}",
        materialized
            .items
            .iter()
            .map(|item| &item.content)
            .collect::<Vec<_>>()
    );
}

/// A terminal semantic transition (supersession) must reach the target
/// wherever its body currently sits. A decision externalized to the store
/// (Cold) and one sitting in the warm buffer are still the same decisions:
/// a later decision on the same entities supersedes them.
#[tokio::test]
async fn supersession_reaches_warm_and_stored_decisions() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 4,
        // First GC pass evicts any unmarked Cooling/Archived item, so the
        // switched-away task's records leave Resident without a long TTL.
        gc_max_generation: 0,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    // A1 (AuthService.rs) is the oldest decision: it overflows to Cold.
    // A3 (CacheStore.rs) is newer: it stays Warm in the buffer.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use CacheStore.rs for caching".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "edited CacheStore.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let (a1_id, a3_id) = {
        let state = engine.state.lock().await;
        (
            state
                .items
                .iter()
                .find(|item| item.content.contains("use AuthService.rs"))
                .expect("decision A1")
                .id,
            state
                .items
                .iter()
                .find(|item| item.content.contains("use CacheStore.rs"))
                .expect("decision A3")
                .id,
        )
    };

    // Switch to task B: A's scopes suspend, A1/A3 cool out of the working
    // set. GC evicts both; the 4-item buffer overflows the oldest (A1) to
    // the store while A3 stays Warm.
    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "investigate the cache miss pattern".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "3", "traced the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = engine.gc().await.unwrap();
    assert!(gc.externalized >= 1, "A1 must reach the store: {gc:?}");
    {
        let state = engine.state.lock().await;
        assert!(state.external.get(a1_id).is_some(), "A1 is a Cold entry");
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == a3_id),
            "A3 stays Warm in the buffer"
        );
    }

    // A later decision on the same entities supersedes each, wherever it
    // sits. The maintain pass applies the queued terminal transitions.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for the cache layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use CacheStore.rs for the read path".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let a1 = state.external.get(a1_id).expect("stored decision entry");
    assert!(
        a1.semantic.is_dead(),
        "the stored decision must be superseded, got {:?}",
        a1.semantic
    );
    let a3 = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == a3_id)
        .expect("warm decision");
    assert!(
        a3.semantic.is_dead(),
        "the warm decision must be superseded, got {:?}",
        a3.semantic
    );
}

/// Error verification must reach an error that left Resident. A
/// successful result on the same entities verifies a Warm error as readily
/// as a resident one.
#[tokio::test]
async fn verification_reaches_warm_errors() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    // A failing tool result persists as a Working Error in task A.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "debug the auth failure".into(),
        })
        .await
        .unwrap();
    failed_observation(&engine, "1", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let error_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("error item")
            .id
    };

    // Task B takes over; A's error cools and is evicted to the buffer.
    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "look at the cache".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "examined the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == error_id),
            "the error must be Warm for this test"
        );
    }

    // A successful result on the same entities verifies the Warm error.
    tool_observation(&engine, "3", "fixed AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let error = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == error_id)
        .expect("warm error");
    assert!(
        error.semantic.is_dead(),
        "the warm error must be verified fixed, got {:?}",
        error.semantic
    );
}

/// A recurring failure supersedes the earlier error wherever it sits — a
/// Warm error is superseded by the next failure on the same site.
#[tokio::test]
async fn recurrence_supersedes_warm_errors() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "debug the auth failure".into(),
        })
        .await
        .unwrap();
    failed_observation(&engine, "1", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let error_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("error item")
            .id
    };

    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "look at the cache".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "examined the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == error_id),
            "the error must be Warm for this test"
        );
    }

    // Same failure site again, in task B: recurrence supersedes the Warm
    // error from task A. Identical content keeps the entity signature
    // (including the line number) identical, as a real recurring failure
    // on the same site would.
    failed_observation(&engine, "3", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let error = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == error_id)
        .expect("warm error");
    assert!(
        error.semantic.is_dead(),
        "the recurring failure must supersede the warm error, got {:?}",
        error.semantic
    );
}

/// Completing a task clears model protections (keep_alive / lease)
/// in every body location, so a completed task cannot keep rooting items
/// through a warm-buffer record.
#[tokio::test]
async fn completed_task_clears_protections_in_every_residency() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_id = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // Protect one resident item (keep_alive + lease), then move it into
    // the warm buffer (an old-checkpoint path a normal GC would never
    // produce, because protected items are roots).
    let protected_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("tool observation")
            .id
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: protected_id,
                keep_alive: true,
            },
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: protected_id,
                turns: 32,
            },
        })
        .await
        .unwrap();
    let warm_id = {
        let mut state = engine.state.lock().await;
        let items = state.items.take_all();
        let mut protected = None;
        let mut rest = Vec::new();
        for item in items {
            if item.id == protected_id {
                protected = Some(item);
            } else {
                rest.push(item);
            }
        }
        state.items.replace_all(rest);
        let protected = protected.expect("the protected item");
        assert!(protected.keep_alive && protected.lease_until_turn.is_some());
        let id = protected.id;
        state.eviction_buffer.push(protected);
        id
    };

    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_id),
            summary: "auth work done".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let warm = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == warm_id)
        .expect("warm protected item");
    assert!(
        !warm.keep_alive && warm.lease_until_turn.is_none(),
        "completed task must clear protections in the warm buffer, got keep_alive={} lease={:?}",
        warm.keep_alive,
        warm.lease_until_turn
    );
}

/// Automatic hot-entity recall of a completed task's records is forbidden
/// without an explicit reason. The hot set alone must not bring finished
/// work back as current truth.
#[tokio::test]
async fn completed_task_blocks_automatic_hot_recall() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    let task_a = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let message_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("task A message")
            .id
    };

    // Complete task A: its scopes close, the working set is evicted, GC
    // moves the archived dialogue to the warm buffer.
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_a),
            summary: "auth fixed".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state
                .eviction_buffer
                .iter()
                .any(|item| item.id == message_id),
            "the completed task's dialogue must be Warm for this test"
        );
    }

    // Task B makes the same entity hot. GC must NOT auto-recall the
    // completed task's record (no explicit reason); an active task's
    // evicted record would be recalled here.
    open_focus(&engine, "auth follow-up").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs again".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.reactivated, 0,
        "a completed task's record must not auto-return on hot entities, got {report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == message_id),
        "completed-task dialogue must stay out of the resident heap"
    );
}

/// Keep-alive accounting is global across body locations — a warm
/// buffer item with keep_alive still consumes the cap.
#[tokio::test]
async fn keep_alive_quota_counts_warm_items() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        max_keep_alive_items: 1,
        ..SimpleContextConfig::default()
    });
    let _task_id = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // A keep-alive item in the warm buffer (old-checkpoint path).
    let _warm_id = {
        let mut state = engine.state.lock().await;
        let items = state.items.take_all();
        let mut protected = None;
        let mut rest = Vec::new();
        for mut item in items {
            if item.kind == ContextKind::ToolObservation {
                item.keep_alive = true;
                protected = Some(item);
            } else {
                rest.push(item);
            }
        }
        state.items.replace_all(rest);
        let protected = protected.expect("a tool observation");
        let id = protected.id;
        state.eviction_buffer.push(protected);
        id
    };

    // The warm buffer item already consumes the single keep_alive slot, so
    // a resident item's keep_alive must be refused by the global quota.
    let target = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("user message")
            .id
    };
    let refused = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: target,
                keep_alive: true,
            },
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("keep_alive"),
        "the warm item must consume the quota, got {refused}"
    );
}

// ---------------------------------------------------------------------------
// Fetch/search/inspect are transient reads; admit re-enters an item under
// its ORIGINAL id with exactly one lifecycle transition; derive persists a
// fact as a NEW item with a DerivedFrom edge to the source ref.
// ---------------------------------------------------------------------------

/// The retrieval surface is a fidelity boundary: `context.admit` must bring
/// an externalized item back into the working set under the same id (never a
/// copy), with exactly one observable lifecycle transition. The result is
/// the same item — `fetch` only reads it, `admit` makes it current again.
#[tokio::test]
async fn admit_externalized_item_preserves_identity_and_produces_one_transition() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
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
    let gc_report = engine.gc().await.unwrap();
    assert!(
        gc_report.externalized >= 1,
        "buffer overflow must externalize: {gc_report:?}"
    );

    // Pick one externalized ref; drain any transitions already pending from
    // the seeding so the admit transition is the only one to count.
    let refs = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(!refs.is_empty(), "the retrieval surface must list refs");
    let target = refs[0].item_id;
    let expected_content = engine
        .fetch_external(target)
        .await
        .unwrap()
        .expect("the ref resolves")
        .content;

    // Admit: same id, one transition, content back in the working set. No
    // maintenance runs between the seed and the admit, so the admitted
    // item's age stays inside the ephemeral TTL and the materializer can
    // still select it.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: target,
                reason: "the model needs this step again".into(),
            },
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::Checkpoint)
        .await
        .unwrap();
    let admits: Vec<_> = report
        .transitions
        .iter()
        .filter(|t| t.item_id == target && t.reason.contains("admitted"))
        .collect();
    assert_eq!(
        admits.len(),
        1,
        "admit must produce exactly one lifecycle transition, got {:?}",
        report.transitions
    );
    assert_eq!(admits[0].to, AttentionState::Active);
    assert_ne!(admits[0].from, AttentionState::Active);

    // The item is back in the working set under its ORIGINAL id — the
    // materializer can select it, and the store no longer owns it.
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue AuthService.rs".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    let back = materialized
        .items
        .iter()
        .find(|item| item.item_id == target)
        .expect("the admitted item is materializable again");
    assert_eq!(
        back.content, expected_content,
        "admit must recover the exact content, not a copy"
    );
    assert_eq!(
        back.attention,
        AttentionState::Active,
        "the admitted item re-enters as an active working-set member"
    );
    assert!(
        engine.fetch_external(target).await.unwrap().is_none(),
        "the admitted item must leave the external map (no duplicate owner)"
    );
}

/// Admit also pulls a warm-buffer item back with its original id, and the
/// transition is observable exactly once.
#[tokio::test]
async fn admit_warm_buffer_item_preserves_identity_and_one_transition() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 64,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "step 0: fix AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    let warm_id = {
        let state = engine.state.lock().await;
        assert!(
            !state.eviction_buffer.is_empty(),
            "the GC must move items to the warm buffer: {report:?}"
        );
        state.eviction_buffer[0].id
    };
    engine
        .maintain(ContextMaintenanceTrigger::Checkpoint)
        .await
        .unwrap();

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: warm_id,
                reason: "the model needs this back".into(),
            },
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::Checkpoint)
        .await
        .unwrap();
    let admits: Vec<_> = report
        .transitions
        .iter()
        .filter(|t| t.item_id == warm_id && t.reason.contains("admitted"))
        .collect();
    assert_eq!(
        admits.len(),
        1,
        "admit must produce exactly one lifecycle transition, got {:?}",
        report.transitions
    );
    let state = engine.state.lock().await;
    assert!(
        !state.eviction_buffer.iter().any(|item| item.id == warm_id),
        "the admitted item must leave the warm buffer"
    );
    let resident = state
        .items
        .iter()
        .find(|item| item.id == warm_id)
        .expect("the admitted item is resident under its original id");
    assert_eq!(resident.attention, AttentionState::Active);
}

/// Terminal semantic states never resurrect: admitting a tombstoned entry
/// is refused with an explainable reason, and the item stays dead.
#[tokio::test]
async fn admit_refused_for_terminal_semantic_item() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    // Two long observations overflow the buffer of 1, forcing the store to
    // externalize (a single evicted item would stay warm instead).
    tool_observation(
        &engine,
        "1",
        &format!("step 0: fix AuthService.rs {}", "z".repeat(200)),
    )
    .await;
    tool_observation(
        &engine,
        "2",
        &format!("step 1: fix CacheStore.rs {}", "z".repeat(200)),
    )
    .await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    let refs = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(!refs.is_empty(), "the entry must be externalized");
    let target = refs[0].item_id;
    // The entry's semantic lifecycle ends while it sits in the store
    // (e.g. Storage GC eligibility): it must stop being retrievable.
    {
        let mut state = engine.state.lock().await;
        state
            .external
            .get_mut(target)
            .expect("the external entry exists")
            .semantic = SemanticState::Tombstoned;
    }
    let refused = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: target,
                reason: "should not resurrect".into(),
            },
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("terminal"),
        "the refusal must be explainable, got {refused}"
    );
    assert!(
        engine.fetch_external(target).await.unwrap().is_none(),
        "a terminal entry must not be retrievable"
    );
}

/// Per-turn quotas bound admit and derive: the model cannot pull the whole
/// external history into the working set in one turn.
#[tokio::test]
async fn admit_and_derive_respect_per_turn_quotas() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 64,
        context_store_dir: Some(dir.path().to_path_buf()),
        max_admits_per_turn: 1,
        max_derived_items_per_turn: 1,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "step 0: fix AuthService.rs").await;
    tool_observation(&engine, "2", "step 1: fix CacheStore.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    let warm_ids: Vec<ContextItemId> = {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.len() >= 2,
            "the test needs two warm items, got {}",
            state.eviction_buffer.len()
        );
        state.eviction_buffer.iter().map(|item| item.id).collect()
    };

    // First admit is granted; the second is refused by the per-turn cap.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: warm_ids[0],
                reason: "first admit".into(),
            },
        })
        .await
        .unwrap();
    let refused = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: warm_ids[1],
                reason: "second admit".into(),
            },
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("admit refused"),
        "the per-turn admit cap must refuse, got {refused}"
    );

    // The derive cap is separate and also refuses after one call.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Derive {
                item_id: warm_ids[1],
                fact: "lesson one".into(),
                reason: "first derive".into(),
            },
        })
        .await
        .unwrap();
    let refused = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Derive {
                item_id: warm_ids[1],
                fact: "lesson two".into(),
                reason: "second derive".into(),
            },
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("derive refused"),
        "the per-turn derive cap must refuse, got {refused}"
    );
}

/// Derive persists a fact as a NEW item with a new id and an explicit
/// `DerivedFrom` edge to the source ref — traceable, but never a copy of
/// the source's identity.
#[tokio::test]
async fn derive_creates_a_new_item_with_a_derived_from_edge() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "step 0: fix AuthService.rs").await;
    let source = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("the source observation")
            .id
    };
    let before = engine.diagnostics().await.unwrap().total_items;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Derive {
                item_id: source,
                fact: "the auth fix landed in AuthService.rs".into(),
                reason: "lesson for the next task".into(),
            },
        })
        .await
        .unwrap();

    {
        let state = engine.state.lock().await;
        let after = state.items.len();
        assert_eq!(
            after,
            before + 1,
            "derive must persist exactly one new item"
        );
        let derived = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Note)
            .expect("the derived item is a Note");
        assert_ne!(
            derived.id, source,
            "the derived item must mint a NEW id, never reuse the source id"
        );
        assert_eq!(
            derived.content, "the auth fix landed in AuthService.rs",
            "the derived item carries the persisted fact"
        );
        assert!(
            derived
                .dependencies
                .iter()
                .any(|edge| edge.kind == DependencyKind::DerivedFrom && edge.target == source),
            "the derived item must carry an explicit DerivedFrom edge, got {:?}",
            derived.dependencies
        );
        assert_eq!(derived.scope, ContextScope::Task);
        assert_eq!(derived.attention, AttentionState::Active);
    }

    // A stale derive target is a silent no-op, like every other directive:
    // the model referenced a ref that left; nothing is minted.
    let before_stale = engine.diagnostics().await.unwrap().total_items;
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Derive {
                item_id: ContextItemId::new(),
                fact: "ghost".into(),
                reason: "stale".into(),
            },
        })
        .await
        .unwrap();
    assert_eq!(
        engine.diagnostics().await.unwrap().total_items,
        before_stale,
        "a stale derive must not mint anything"
    );
}

/// The startup reconcile, driven through the public engine API, converges a
/// crash-injected store: an uncommitted orphan blob is rebuilt into an
/// entry, a stale duplicate of resident content is reclaimed, a damaged
/// blob is quarantined (evidence preserved), and an abandoned temp file is
/// removed — with every action surfaced in the `StoreReconcileReport`.
#[tokio::test]
async fn reconcile_store_converges_a_crash_injected_directory() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 64,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::Pin {
            content: "AuthService reconcile sentinel".into(),
            kind: ContextKind::Constraint,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for i in 0..70 {
        tool_observation(
            &engine,
            &i.to_string(),
            &format!("step {i}: fix AuthService.rs"),
        )
        .await;
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = engine.gc().await.unwrap();
    assert!(
        gc.externalized > 0,
        "buffer overflow must externalize: {gc:?}"
    );

    // Crash-inject four states. Orphan: a valid blob under a fresh id, as
    // if the rename landed but the map commit never ran. Stale: the same
    // trick under a *resident* id (the pin), as if a recall commit landed
    // but the post-commit blob delete never ran. Damaged: garbage under a
    // formal name. Temp: an abandoned atomic write.
    let blobs: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .collect();
    assert!(!blobs.is_empty(), "the engine wrote real blobs");
    let source = std::fs::read(blobs[0].path()).unwrap();
    let orphan_id = ContextItemId::new();
    let mut orphan_value: serde_json::Value = serde_json::from_slice(&source).unwrap();
    orphan_value["id"] = serde_json::to_value(orphan_id).unwrap();
    std::fs::write(
        dir.path().join(format!("{orphan_id}.json")),
        serde_json::to_vec(&orphan_value).unwrap(),
    )
    .unwrap();

    let residents = engine.inspect(100).await.unwrap();
    assert!(
        !residents.is_empty(),
        "the pinned constraint stays resident"
    );
    let resident_id = residents[0].id;
    let mut stale_value: serde_json::Value = serde_json::from_slice(&source).unwrap();
    stale_value["id"] = serde_json::to_value(resident_id).unwrap();
    std::fs::write(
        dir.path().join(format!("{resident_id}.json")),
        serde_json::to_vec(&stale_value).unwrap(),
    )
    .unwrap();

    let damaged_id = ContextItemId::new();
    std::fs::write(dir.path().join(format!("{damaged_id}.json")), b"garbage{{{").unwrap();
    let temp_id = ContextItemId::new();
    std::fs::write(dir.path().join(format!("{temp_id}.tmp")), b"partial").unwrap();

    let report = engine.reconcile_store().await.unwrap();
    assert_eq!(report.rebuilt, 1, "orphan rebuilt: {report:?}");
    assert_eq!(report.deleted_stale, 1, "stale reclaimed: {report:?}");
    assert_eq!(report.quarantined, 1, "damaged quarantined: {report:?}");
    assert_eq!(report.temp_cleaned, 1, "temp cleaned: {report:?}");
    assert_eq!(report.io_errors, 0);

    // The rebuilt orphan is retrievable content again; the reclaimed id has
    // neither a blob nor a duplicate entry.
    assert!(
        engine.fetch_external(orphan_id).await.unwrap().is_some(),
        "the rebuilt orphan is retrievable"
    );
    assert!(
        !dir.path().join(format!("{resident_id}.json")).exists(),
        "the reclaimed stale blob is gone"
    );
    let search = engine
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
        search.iter().all(|e| e.item_id != resident_id),
        "the reclaimed id is not duplicated in the map"
    );
}

/// The operation gate serializes the multi-phase/whole-state operations.
/// While the gate is held — exactly as when a sibling operation is
/// mid-flight between its plan and its commit — a GC, storage GC, store
/// reconcile, checkpoint or restore must block. Releasing the gate lets
/// every one of them run to completion. Without the gate a restore could
/// replace the whole state between a GC's plan and its commit, and the
/// stale plan would land on top of the restored state.
#[tokio::test]
async fn multi_phase_operations_are_serialized_by_the_operation_gate() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }));
    let empty_checkpoint = engine.checkpoint().await.unwrap();

    // Hold the gate from the test task: every multi-phase/whole-state
    // operation must wait for it, exactly as it would wait for a sibling
    // operation currently mid-flight.
    let _gate = engine.op_gate.lock().await;

    let gc = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.gc().await })
    };
    let storage_gc = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.storage_gc().await })
    };
    let reconcile = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.reconcile_store().await })
    };
    let checkpoint = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.checkpoint().await })
    };
    let restore = {
        let engine = Arc::clone(&engine);
        let checkpoint = empty_checkpoint.clone();
        tokio::spawn(async move { engine.restore(checkpoint).await })
    };

    // None of the five may complete while the gate is held.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!gc.is_finished(), "gc must wait for the operation gate");
    assert!(
        !storage_gc.is_finished(),
        "storage gc must wait for the operation gate"
    );
    assert!(
        !reconcile.is_finished(),
        "store reconcile must wait for the operation gate"
    );
    assert!(
        !checkpoint.is_finished(),
        "checkpoint must wait for the operation gate"
    );
    assert!(
        !restore.is_finished(),
        "restore must wait for the operation gate"
    );

    drop(_gate);
    gc.await.unwrap().unwrap();
    storage_gc.await.unwrap().unwrap();
    reconcile.await.unwrap().unwrap();
    checkpoint.await.unwrap().unwrap();
    restore.await.unwrap().unwrap();
}

/// A checkpoint that violates the structural invariants must be refused on
/// restore with an explicit error — never silently adopted. The engine
/// maintains these invariants at runtime, so a violating checkpoint is
/// corrupt or hostile rather than a legacy format.
#[tokio::test]
async fn restore_rejects_checkpoints_that_violate_structural_invariants() {
    let config = SimpleContextConfig::default();

    let make_item = |state: &crate::engine::State, id: ContextItemId, content: &str| {
        let mut item = crate::item::make_item(
            state,
            &config,
            content.into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.id = id;
        item
    };

    // Duplicate id inside the heap: the id index hides it (last-wins), so
    // only a raw-vector scan catches it.
    let mut dup_heap = crate::engine::State::default();
    let id = ContextItemId::new();
    let item = make_item(&dup_heap, id, "first body");
    dup_heap.items.replace_all(vec![item.clone(), item]);
    expect_restore_rejected(&dup_heap, "more than once").await;

    // An id owned by both the heap and the eviction buffer.
    let mut cross = crate::engine::State::default();
    let id = ContextItemId::new();
    let item = make_item(&cross, id, "shared body");
    cross.items.replace_all(vec![item.clone()]);
    cross.eviction_buffer.push(item);
    expect_restore_rejected(&cross, "owned by both").await;

    // A scope whose parent is missing from the tree.
    let mut broken_parent = crate::engine::State::default();
    let missing = ScopeId::new();
    broken_parent.scopes.push(agent_contracts::Scope {
        id: ScopeId::new(),
        parent: Some(missing),
        kind: ScopeKind::Task,
        state: ScopeState::Active,
        task_id: None,
        goal: None,
        opened_tick: 1,
        last_active_tick: 1,
        closed_tick: None,
    });
    expect_restore_rejected(&broken_parent, "missing parent").await;

    // An item referencing a scope that does not exist.
    let mut missing_scope = crate::engine::State::default();
    let mut item = make_item(&missing_scope, ContextItemId::new(), "orphan body");
    item.scope_id = Some(ScopeId::new());
    missing_scope.items.replace_all(vec![item]);
    expect_restore_rejected(&missing_scope, "missing scope").await;

    // The clean control: a default state still round-trips.
    let value = crate::checkpoint::serialize(&crate::engine::State::default()).unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine.restore(value).await.unwrap();
}

async fn expect_restore_rejected(state: &crate::engine::State, needle: &str) {
    let value = crate::checkpoint::serialize(state).unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let error = engine.restore(value).await.unwrap_err().to_string();
    assert!(
        error.contains("checkpoint restore validation") && error.contains(needle),
        "expected a validation error mentioning '{needle}', got: {error}"
    );
}

/// A dependency demoted to the warm buffer is still recalled when a live
/// root depends on it — the mark phase and the reactivate phase share one
/// universe. Regression: the mark traversal only followed edges through
/// the heap and reactivation only recalled hot-entity/score matches, so a
/// demoted dependency was marked but never brought back.
#[tokio::test]
async fn demoted_dependency_is_recalled_because_a_live_root_depends_on_it() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let evidence_id = {
        let mut state = engine.state.lock().await;
        // The evidence lives in the warm buffer with no hot entities and a
        // low score: nothing but the dependency edge marks it.
        let mut evidence = crate::item::make_item(
            &state,
            &engine.config,
            "cold evidence log".into(),
            ContextKind::ToolObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.1,
            None,
        );
        let evidence_id = evidence.id;
        evidence.attention = AttentionState::Archived;
        evidence.residency = ContextResidency::Warm;
        evidence.evicted_at_tick = Some(0);
        state.eviction_buffer.push(evidence);

        // A pinned live decision in the heap cites the evidence.
        let mut root = crate::item::make_item(
            &state,
            &engine.config,
            "live decision citing the evidence".into(),
            ContextKind::Decision,
            ContextScope::Pinned,
            ContextRetention::Pinned,
            0.9,
            None,
        );
        root.dependencies.push(DependencyEdge {
            target: evidence_id,
            kind: DependencyKind::EvidenceFor,
        });
        state.items.replace_all(vec![root]);
        evidence_id
    };

    let report = engine.gc().await.unwrap();
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.item_id == evidence_id && r.reason.contains("dependency of a marked root")),
        "the demoted dependency must be recalled because its root depends on it: {:?}",
        report.reactivations
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.items.iter().any(|item| item.id == evidence_id),
            "the recalled evidence must be back in the heap"
        );
    }
}

/// An external-only state (nothing resident) still runs a full GC pass so
/// Cold entries age toward External — the pass-skip check must not treat
/// an empty heap and buffer as "nothing to do" while the external map
/// still holds entries that need aging and recall.
#[tokio::test]
async fn external_only_state_still_ages_cold_entries_on_full_gc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        gc_external_ttl_generations: 1,
        ..SimpleContextConfig::default()
    });
    {
        let mut state = engine.state.lock().await;
        let item = crate::item::make_item(
            &state,
            &engine.config,
            "stored decision body".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        let entry = crate::store::to_external_entry(
            &item,
            agent_contracts::ContextRef {
                uri: format!("context://run/{}", item.id),
                item_id: item.id,
                kind: ContextKind::Decision,
                scope: ContextScope::Task,
                summary: "stored decision".into(),
                created_tick: 0,
            },
            0,
            0,
            None,
        );
        state.external.push(entry);
    }

    let report = engine.gc().await.unwrap();
    assert!(
        report.aged_external >= 1,
        "a Cold entry must age to External when the state is external-only: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        let entry = state.external.iter().next().unwrap();
        assert_eq!(
            entry.residency,
            ContextResidency::External,
            "the Cold entry must have aged to External"
        );
    }
}

/// A durable outcome of a closing scope is promoted even when it was
/// already evicted to the warm buffer — promotion follows the item, not
/// just the resident heap, and a promoted item becomes resident again.
#[tokio::test]
async fn warm_buffer_durable_outcome_is_promoted_on_scope_close() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let (task_scope_id, focus_scope_id, item_id) = {
        let mut state = engine.state.lock().await;
        let session_id = crate::scope::ensure_session(&mut state);
        let task_id = TaskId::new();
        let task_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(session_id),
            kind: ScopeKind::Task,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 1,
            last_active_tick: 1,
            closed_tick: None,
        });
        let focus_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(task_scope_id),
            kind: ScopeKind::Focus,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 2,
            last_active_tick: 2,
            closed_tick: None,
        });
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "durable task decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.9,
            None,
        );
        item.scope_id = Some(focus_scope_id);
        item.attention = AttentionState::Archived;
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(1);
        let item_id = item.id;
        state.eviction_buffer.push(item);
        (task_scope_id, focus_scope_id, item_id)
    };

    engine.close_scope(focus_scope_id).await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.items.iter().any(|item| item.id == item_id),
        "a promoted buffer outcome must be resident again"
    );
    assert!(
        state.eviction_buffer.iter().all(|item| item.id != item_id),
        "a promoted buffer outcome must leave the eviction buffer"
    );
    let promoted = state.items.iter().find(|item| item.id == item_id).unwrap();
    assert_eq!(
        promoted.scope_id,
        Some(task_scope_id),
        "the durable outcome must promote to the task scope"
    );
    assert_eq!(promoted.scope, ContextScope::Task);
    assert!(
        promoted
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
        "the promotion must be labeled"
    );
}

/// A durable outcome already externalized to the context store is promoted
/// by the same scope-close pass as resident and warm bodies: the membership
/// identity re-stamps to the nearest open ancestor (focus close -> task
/// scope, task close -> session), retention upgrades, the move is labeled
/// and attention moves to Active exactly like a resident promotion. Working
/// bodies and semantically dead entries stay where they are; legacy entries
/// without a scope stamp fall back to the task id.
#[tokio::test]
async fn external_durable_outcome_is_promoted_on_scope_close() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let (session_id, task_scope_id, focus_scope_id, durable_id, legacy_id, working_id, other_id) = {
        let mut state = engine.state.lock().await;
        let session_id = crate::scope::ensure_session(&mut state);
        let task_id = TaskId::new();
        let task_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(session_id),
            kind: ScopeKind::Task,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 1,
            last_active_tick: 1,
            closed_tick: None,
        });
        let focus_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(task_scope_id),
            kind: ScopeKind::Focus,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 2,
            last_active_tick: 2,
            closed_tick: None,
        });
        let reference = |id: ContextItemId| agent_contracts::ContextRef {
            uri: format!("context://run/{id}"),
            item_id: id,
            kind: ContextKind::Decision,
            scope: ContextScope::Task,
            summary: "stored".into(),
            created_tick: 0,
        };

        let mut durable = crate::item::make_item(
            &state,
            &engine.config,
            "stored durable decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.9,
            None,
        );
        durable.task_id = Some(task_id);
        durable.scope_id = Some(focus_scope_id);
        durable.attention = AttentionState::Archived;
        let durable_id = durable.id;
        state.external.push(crate::store::to_external_entry(
            &durable,
            reference(durable.id),
            1,
            1,
            None,
        ));

        // Working body of the same focus: durable-outcome promotion must
        // leave it alone — it is not a durable outcome of the scope.
        let mut working = crate::item::make_item(
            &state,
            &engine.config,
            "stored working note".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        working.task_id = Some(task_id);
        working.scope_id = Some(focus_scope_id);
        working.attention = AttentionState::Archived;
        let working_id = working.id;
        state.external.push(crate::store::to_external_entry(
            &working,
            reference(working.id),
            1,
            1,
            None,
        ));

        // Legacy entry that predates the scope stamp: the task id decides.
        let mut legacy = crate::item::make_item(
            &state,
            &engine.config,
            "legacy stored decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        legacy.task_id = Some(task_id);
        legacy.scope_id = None;
        legacy.attention = AttentionState::Archived;
        let legacy_id = legacy.id;
        state.external.push(crate::store::to_external_entry(
            &legacy,
            reference(legacy.id),
            1,
            1,
            None,
        ));

        // Durable entry of a *different* task: no task match, so neither
        // close may touch it.
        let mut other = crate::item::make_item(
            &state,
            &engine.config,
            "other task decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        other.task_id = Some(TaskId::new());
        other.scope_id = None;
        other.attention = AttentionState::Archived;
        let other_id = other.id;
        state.external.push(crate::store::to_external_entry(
            &other,
            reference(other.id),
            1,
            1,
            None,
        ));

        (
            session_id,
            task_scope_id,
            focus_scope_id,
            durable_id,
            legacy_id,
            working_id,
            other_id,
        )
    };

    // Focus close: durable and legacy promote to the task scope; the
    // working body and the other-task entry stay untouched.
    let transitions = engine.close_scope(focus_scope_id).await.unwrap();
    {
        let state = engine.state.lock().await;
        let durable = state.external.get(durable_id).unwrap();
        assert_eq!(
            durable.scope_id,
            Some(task_scope_id),
            "a stored durable outcome must promote to the task scope on focus close"
        );
        assert_eq!(durable.scope, ContextScope::Task);
        assert_eq!(
            durable.attention,
            AttentionState::Active,
            "external promotion must move attention to Active like a resident promotion"
        );
        assert!(
            durable
                .tags
                .iter()
                .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
            "the external promotion must be labeled"
        );
        let legacy = state.external.get(legacy_id).unwrap();
        assert_eq!(
            legacy.scope_id,
            Some(task_scope_id),
            "a legacy entry without a scope stamp must promote by task id"
        );
        assert_eq!(
            state.external.get(working_id).unwrap().scope_id,
            Some(focus_scope_id),
            "a working body is not a durable outcome and must not be promoted"
        );
        assert_eq!(
            state.external.get(other_id).unwrap().scope_id,
            None,
            "an entry of another task must not be promoted by this close"
        );
    }
    assert!(
        transitions.iter().any(|t| {
            t.item_id == durable_id
                && t.from == AttentionState::Archived
                && t.to == AttentionState::Active
        }),
        "the durable external promotion must be observable as a transition"
    );
    assert!(
        transitions.iter().any(|t| t.item_id == legacy_id),
        "the legacy external promotion must be observable as a transition"
    );

    // Task close: the same entries promote once more, to the session scope;
    // the working body still stays behind in the closed focus.
    let transitions = engine.close_scope(task_scope_id).await.unwrap();
    {
        let state = engine.state.lock().await;
        let durable = state.external.get(durable_id).unwrap();
        assert_eq!(
            durable.scope_id,
            Some(session_id),
            "a stored durable outcome must promote to the session scope on task close"
        );
        assert_eq!(durable.scope, ContextScope::Session);
        let legacy = state.external.get(legacy_id).unwrap();
        assert_eq!(legacy.scope_id, Some(session_id));
        assert_eq!(
            state.external.get(working_id).unwrap().scope_id,
            Some(focus_scope_id),
            "the working body must still point at the closed focus"
        );
        assert_eq!(state.external.get(other_id).unwrap().scope_id, None);
    }
    assert!(
        transitions.iter().all(|t| t.item_id != durable_id),
        "an already-active external entry must not emit a second transition \
         (promotion records attention changes, like the resident pass)"
    );
}

/// A task close queues the task scope and *every* open descendant — the
/// focus episode and the tool frames nested under it — so a deep descendant
/// never keeps pointing at scopes that are already closed. The durable
/// outcome of the deepest tool frame still promotes to the nearest open
/// ancestor (the session, once task and focus are closed).
#[tokio::test]
async fn task_close_closes_deep_descendants_and_promotes_their_outcomes() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let (session_id, task_scope_id, focus_scope_id, tool_scope_id, item_id) = {
        let mut state = engine.state.lock().await;
        let session_id = crate::scope::ensure_session(&mut state);
        let task_id = TaskId::new();
        let task_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(session_id),
            kind: ScopeKind::Task,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 1,
            last_active_tick: 1,
            closed_tick: None,
        });
        let focus_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(task_scope_id),
            kind: ScopeKind::Focus,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 2,
            last_active_tick: 2,
            closed_tick: None,
        });
        let tool_scope_id = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(focus_scope_id),
            kind: ScopeKind::Tool,
            state: ScopeState::Active,
            task_id: Some(task_id),
            goal: None,
            opened_tick: 3,
            last_active_tick: 3,
            closed_tick: None,
        });
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "durable tool outcome".into(),
            ContextKind::Decision,
            ContextScope::Turn,
            ContextRetention::Durable,
            0.9,
            None,
        );
        item.task_id = Some(task_id);
        item.scope_id = Some(tool_scope_id);
        item.attention = AttentionState::Archived;
        let item_id = item.id;
        state.items.push(item);
        crate::scope::queue_task_scope_close(&mut state, task_id);
        (
            session_id,
            task_scope_id,
            focus_scope_id,
            tool_scope_id,
            item_id,
        )
    };

    let transitions = {
        let mut state = engine.state.lock().await;
        crate::scope::drain_closed_scopes(&mut state, 1)
    };
    {
        let state = engine.state.lock().await;
        let closed = |id: ScopeId| {
            state
                .scopes
                .by_id(id)
                .is_some_and(|scope| scope.state == ScopeState::Closed)
        };
        assert!(
            closed(task_scope_id),
            "the task scope itself must be closed by the queued close"
        );
        assert!(
            closed(focus_scope_id),
            "the focus descendant must be closed with the task"
        );
        assert!(
            closed(tool_scope_id),
            "a deep tool-frame descendant must be closed with the task, \
             not left pointing at closed scopes"
        );
        let promoted = state.items.iter().find(|item| item.id == item_id).unwrap();
        assert_eq!(
            promoted.scope_id,
            Some(session_id),
            "the tool frame's durable outcome must promote to the nearest open \
             ancestor once task and focus are closed"
        );
        assert_eq!(promoted.scope, ContextScope::Session);
        assert!(
            promoted
                .tags
                .iter()
                .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
            "the promotion must be labeled"
        );
    }
    assert!(
        transitions
            .iter()
            .any(|t| t.item_id == item_id && t.to == AttentionState::Active),
        "the promotion must be observable as a transition"
    );
}

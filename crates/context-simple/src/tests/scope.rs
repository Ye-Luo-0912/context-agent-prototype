use agent_contracts::{
    AttentionState, ContextEngine, ContextHints, ContextIngress, ContextItem, ContextItemId,
    ContextKind, ContextMaintenanceTrigger, ContextQuery, ContextRetention, ContextScope,
    FocusState, LifecycleLabel, ScopeKind, ScopeState, SemanticState, TaskId, ToolOutput,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

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
                anchor_roots: Vec::new(),
                task: None,
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
            last_selected_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some("pinned".into()),
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: Some("pinned".into()),
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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

use agent_contracts::{
    AttentionState, ContextEngine, ContextHints, ContextIngress, ContextItem, ContextItemId,
    ContextKind, ContextQuery, ContextRetention, ContextScope, SemanticState, ToolOutput,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

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
            last_selected_turn: 1,
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: vec![agent_contracts::DependencyEdge::shares(dep_id)],
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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
            last_selected_turn: 1,
            dependencies: Vec::new(),
            tags: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            source: None,
            residency: agent_contracts::ContextResidency::Resident,
            gc_generation: 0,
            evicted_at_tick: None,
            entities: Vec::new(),
            file_path: None,
            file_revision: None,
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

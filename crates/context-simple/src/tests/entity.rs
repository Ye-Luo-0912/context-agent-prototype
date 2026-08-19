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

    // A structured resource touch extends tool-hot (newest first). Stdout
    // in model_content must not.
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "CacheStore.rs is hot now".into(),
                artifact_ref: None,
                metadata: serde_json::json!({"path": "src/CacheStore.rs"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.hot_entities.first().map(String::as_str),
            Some("src/CacheStore.rs"),
            "most recently touched resource must lead"
        );
        assert!(state.hot_entities.contains(&"AuthService.rs".to_string()));
    }

    // The next user message replaces user-hot; tool-hot survives until TTL.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "unrelated plain words".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state
                .hot_entities
                .contains(&"src/CacheStore.rs".to_string()),
            "tool-hot must survive the next user turn (ttl=2): {:?}",
            state.hot_entities
        );
        assert!(
            !state.hot_entities.contains(&"AuthService.rs".to_string()),
            "user-hot must reset on a new user message"
        );
    }

    engine
        .ingest(ContextIngress::UserMessage {
            content: "still unrelated".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.hot_entities.is_empty(),
            "tool-hot expires after its ttl: {:?}",
            state.hot_entities
        );
    }
}

#[tokio::test]
async fn failed_tool_observation_does_not_heat_candidate_entities() {
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
                tool_name: "edit.replace".into(),
                ok: false,
                summary: "no_exact_match".into(),
                model_content: "candidate:\nsrc/foo.rs\nsrc/bar.rs".into(),
                artifact_ref: None,
                metadata: serde_json::json!({"failure_class": "no_exact_match"}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(state.hot_entities.contains(&"AuthService.rs".to_string()));
        assert!(
            state
                .items
                .iter()
                .any(|item| item.kind == ContextKind::Error),
            "failed observations still persist as typed Error items"
        );
        assert!(
            !state.hot_entities.iter().any(|e| e.contains("foo.rs")),
            "failed edit candidates must not become hot: {:?}",
            state.hot_entities
        );
        assert!(
            !state.hot_entities.iter().any(|e| e.contains("bar.rs")),
            "failed edit candidates must not become hot: {:?}",
            state.hot_entities
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
                metadata: serde_json::json!({"path": "AuthService.rs"}),
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
            dependencies: vec![agent_contracts::DependencyEdge::continuation(dep_id)],
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
async fn primary_selection_keeps_full_budget_without_continuation() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        let item = ContextItem {
            id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            content: "noise data ".repeat(800),
            kind: ContextKind::Note,
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
        state.items.push(item);
    }
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "go".into(),
            budget_tokens: 2500,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        snapshot.selected.len(),
        1,
        "without Continuation the full budget is primary selection; a 1024 reserve would drop this ~2000 token item: {:?}",
        snapshot.selected
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
            dependencies: vec![agent_contracts::DependencyEdge::continuation(dep_id)],
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
            dependencies: vec![agent_contracts::DependencyEdge::continuation(dep_id)],
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

    // A huge pinned constraint continued by the observation. Entity
    // overlap would only mint SharesEntities (affinity, not a prompt
    // body); the budget test needs a Continuation edge.
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
    {
        let mut state = engine.state.lock().await;
        let pin_id = state
            .items
            .iter()
            .find(|item| item.retention == ContextRetention::Pinned)
            .expect("pin")
            .id;
        for item in state.items.iter_mut() {
            if item.kind == ContextKind::ToolObservation {
                item.dependencies = vec![agent_contracts::DependencyEdge::continuation(pin_id)];
            }
        }
    }

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

fn hub_and_dep(
    hub_kind: ContextKind,
    edge: agent_contracts::DependencyEdge,
    dep_content: String,
    dep_attention: AttentionState,
) -> (ContextItem, ContextItem) {
    let hub_id = ContextItemId::new();
    let dep_id = edge.target;
    let hub = ContextItem {
        id: hub_id,
        task_id: None,
        scope_id: None,
        content: "hub data ".repeat(400),
        kind: hub_kind,
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
        dependencies: vec![edge],
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
        content: dep_content,
        kind: ContextKind::UserMessage,
        scope: ContextScope::Turn,
        retention: ContextRetention::Working,
        attention: dep_attention,
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
    (hub, dep)
}

#[tokio::test]
async fn shares_entities_does_not_expand_into_the_prompt() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let dep_id = ContextItemId::new();
    let (hub, dep) = hub_and_dep(
        ContextKind::UserMessage,
        agent_contracts::DependencyEdge::shares(dep_id),
        "raw overlap body".into(),
        AttentionState::Archived,
    );
    {
        let mut state = engine.state.lock().await;
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
        "SharesEntities is affinity, not a prompt citation: {:?}",
        snapshot
            .selected
            .iter()
            .map(|selection| &selection.reason)
            .collect::<Vec<_>>()
    );
    assert!(
        !snapshot.items.iter().any(|item| item.item_id == dep_id),
        "the overlap target must stay out of the working set"
    );
}

#[tokio::test]
async fn derived_from_does_not_reexpand_compacted_sources() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let dep_id = ContextItemId::new();
    let (hub, dep) = hub_and_dep(
        ContextKind::Summary,
        agent_contracts::DependencyEdge::derived_from(dep_id),
        "raw episode that compaction already folded".into(),
        AttentionState::Archived,
    );
    {
        let mut state = engine.state.lock().await;
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
        snapshot
            .items
            .iter()
            .any(|item| item.kind == ContextKind::Summary),
        "the compact card itself may be selected"
    );
    assert!(
        !snapshot
            .selected
            .iter()
            .any(|selection| selection.reason.contains("included as dependency")),
        "DerivedFrom is provenance, not a prompt citation: {:?}",
        snapshot
            .selected
            .iter()
            .map(|selection| &selection.reason)
            .collect::<Vec<_>>()
    );
    assert!(
        !snapshot.items.iter().any(|item| item.item_id == dep_id),
        "compaction sources must not re-enter through DerivedFrom"
    );
}

fn fs_read(call_id: &str, path: &str) -> ToolOutput {
    ToolOutput {
        call_id: call_id.into(),
        tool_name: "fs.read".into(),
        ok: true,
        summary: "read".into(),
        model_content: "     1 | fn body() {}".into(),
        artifact_ref: None,
        metadata: serde_json::json!({"path": path}),
    }
}

#[tokio::test]
async fn shell_stdout_does_not_heat_entities() {
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
                summary: "ok".into(),
                model_content: "compiled AuthService.rs and CacheStore.rs".into(),
                artifact_ref: None,
                metadata: serde_json::json!({}),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        !state.hot_entities.iter().any(|e| e.contains("AuthService")),
        "stdout must not seed tool-hot: {:?}",
        state.hot_entities
    );
    let observation = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::ToolObservation)
        .expect("observation");
    assert!(
        observation.entities.is_empty(),
        "pathless stdout must not become the observation's entity signature: {:?}",
        observation.entities
    );
}

#[tokio::test]
async fn stamped_shell_path_does_not_supersede_prior_shell_observation() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "touch AuthService.rs".into(),
        })
        .await
        .unwrap();
    for (id, body) in [("1", "first shell log"), ("2", "second shell log")] {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: id.into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: body.into(),
                    artifact_ref: None,
                    metadata: serde_json::json!({ "path": "AuthService.rs" }),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let tools: Vec<_> = state
        .items
        .iter()
        .filter(|item| item.kind == ContextKind::ToolObservation)
        .collect();
    assert_eq!(tools.len(), 2, "both shell observations stay in the heap");
    assert!(
        tools.iter().all(|item| item.semantic.is_live()),
        "a stamped path is identity, not a file-body supersession: {:?}",
        tools.iter().map(|item| &item.semantic).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn patch_files_array_stamps_all_paths_into_identity() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "apply the split".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "patch-1".into(),
                tool_name: "edit.patch".into(),
                ok: true,
                summary: "applied".into(),
                model_content: "patch applied: 2 file(s)".into(),
                artifact_ref: None,
                metadata: serde_json::json!({
                    "files": [
                        {"path": "src/auth.rs", "revision": "aa"},
                        {"path": "src/billing.rs", "revision": "bb"}
                    ]
                }),
            },
            scope_id: None,
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.hot_entities.iter().any(|e| e == "src/auth.rs")
            && state.hot_entities.iter().any(|e| e == "src/billing.rs"),
        "multi-file patch must heat every stamped path: {:?}",
        state.hot_entities
    );
    let observation = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::ToolObservation)
        .expect("observation");
    assert_eq!(observation.file_path.as_deref(), Some("src/auth.rs"));
    assert!(
        observation.entities.iter().any(|e| e == "src/auth.rs")
            && observation.entities.iter().any(|e| e == "src/billing.rs"),
        "identity is every stamped path, not stdout: {:?}",
        observation.entities
    );
}

#[tokio::test]
async fn working_set_prose_does_not_heat_entities() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "start".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::WorkingSetSignal {
            resources: Vec::new(),
            content: "discovered AuthService.rs and CacheStore.rs".into(),
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.hot_entities.is_empty(),
        "legacy WorkingSetSignal content must not heat: {:?}",
        state.hot_entities
    );
}

#[tokio::test]
async fn fs_read_reread_classes_and_selected_attribution() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    super::harness::open_focus(&engine, "fix files").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect src/a.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("1", "src/a.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.reread_first_read, 1);
        assert_eq!(state.reread_resident_unselected, 0);
    }
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("2", "src/a.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.reread_resident_unselected, 1);
        assert_eq!(state.reread_previously_selected, 0);
    }
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "inspect src/a.rs".into(),
            budget_tokens: 10_000,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        snapshot
            .selected
            .iter()
            .any(|sel| sel.kind == Some(ContextKind::ToolObservation)
                && sel.source.as_deref() == Some("tool:fs.read")),
        "selected tokens must carry kind/source: {:?}",
        snapshot.selected
    );
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("3", "src/a.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.reread_previously_selected, 1,
            "a selected file body reread is previously-selected"
        );
    }
}

#[tokio::test]
async fn fs_read_reread_warm_and_stored() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    super::harness::open_focus(&engine, "fix files").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect src/warm.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("1", "src/warm.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let Some(pos) = items
            .iter()
            .position(|item| item.file_path.as_deref() == Some("src/warm.rs"))
        else {
            panic!("the read must be resident");
        };
        let item = items.remove(pos);
        state.items.replace_all(items);
        state.eviction_buffer.push(item);
    }
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("2", "src/warm.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.reread_warm, 1);
    }

    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("3", "src/stored.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let Some(pos) = items
            .iter()
            .position(|item| item.file_path.as_deref() == Some("src/stored.rs"))
        else {
            panic!("the stored-path read must be resident");
        };
        let item = items.remove(pos);
        state.items.replace_all(items);
        let entry = crate::store::to_external_entry(
            &item,
            agent_contracts::ContextRef {
                uri: format!("context://run/{}", item.id),
                item_id: item.id,
                kind: item.kind,
                scope: item.scope,
                summary: "stored".into(),
                created_tick: item.created_tick,
            },
            0,
            0,
            None,
        );
        state.external.push(entry);
    }
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("4", "src/stored.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert_eq!(state.reread_stored, 1);
}

#[tokio::test]
async fn recent_file_bodies_cap_and_one_round_lease() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        recent_file_bodies: 1,
        recent_file_body_lease_turns: 1,
        ..SimpleContextConfig::default()
    });
    super::harness::open_focus(&engine, "fix files").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "read files".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("1", "src/old.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            output: fs_read("2", "src/new.rs"),
            scope_id: None,
        })
        .await
        .unwrap();
    let new_id;
    let old_id;
    {
        let state = engine.state.lock().await;
        old_id = state
            .items
            .iter()
            .find(|item| item.file_path.as_deref() == Some("src/old.rs"))
            .map(|item| item.id)
            .expect("old body");
        new_id = state
            .items
            .iter()
            .find(|item| item.file_path.as_deref() == Some("src/new.rs"))
            .map(|item| item.id)
            .expect("new body");
        let latest = state.latest_file_body_ids();
        assert!(latest.contains(&new_id), "cap=1 keeps the newest body");
        assert!(
            !latest.contains(&old_id),
            "cap=1 drops the older path: {latest:?}"
        );
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: "next turn".into(),
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.latest_file_body_ids().is_empty(),
        "one-round lease expires at the next user turn"
    );
}

#[tokio::test]
async fn checked_file_body_is_priced_as_a_descriptor() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    let (body_id, heap_body) = {
        let mut state = engine.state.lock().await;
        let mut body = crate::item::make_item(
            &state,
            &engine.config,
            "x".repeat(4_000),
            ContextKind::ToolObservation,
            ContextScope::Session,
            ContextRetention::Working,
            0.58,
            Some("tool:fs.read".into()),
        );
        body.scope_id = None;
        body.file_path = Some("src/big.rs".into());
        body.file_revision = Some("aaa".into());
        let mut note = crate::item::make_item(
            &state,
            &engine.config,
            "keep this Constraint about the public API".into(),
            ContextKind::Constraint,
            ContextScope::Session,
            ContextRetention::Working,
            0.9,
            None,
        );
        note.scope_id = None;
        let id = body.id;
        let heap_body = body.content.clone();
        state.items.push(body);
        state.items.push(note);
        (id, heap_body)
    };
    let without = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 200,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !without.items.iter().any(|item| item.item_id == body_id),
        "full file body must not fit a 200-token budget"
    );
    assert!(
        without
            .items
            .iter()
            .any(|item| item.kind == ContextKind::Constraint),
        "the small constraint must still fit"
    );

    let with = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 200,
            hints: ContextHints {
                checked_files: vec!["src/big.rs@aaa".into()],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let selected = with
        .items
        .iter()
        .find(|item| item.item_id == body_id)
        .expect("descriptor-priced file body must fit");
    assert_eq!(selected.content, "src/big.rs@aaa");
    assert!(
        with.items
            .iter()
            .any(|item| item.kind == ContextKind::Constraint),
        "descriptor pricing must leave room for the constraint"
    );
    assert!(
        with.selected
            .iter()
            .any(|sel| sel.item_id == body_id && sel.reason.contains("path already checked")),
        "reason must say the body was omitted: {:?}",
        with.selected
    );
    let state = engine.state.lock().await;
    let heap = state
        .items
        .iter()
        .find(|item| item.id == body_id)
        .expect("heap row");
    assert_eq!(heap.content, heap_body, "heap body stays intact");
}

#[tokio::test]
async fn checked_stamped_shell_is_priced_as_a_descriptor() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    let (log_id, heap_body) = {
        let mut state = engine.state.lock().await;
        let mut log = crate::item::make_item(
            &state,
            &engine.config,
            "x".repeat(4_000),
            ContextKind::ToolObservation,
            ContextScope::Session,
            ContextRetention::Working,
            0.58,
            Some("tool:shell.exec".into()),
        );
        log.scope_id = None;
        log.file_path = Some("src/big.rs".into());
        log.file_revision = Some("aaa".into());
        let mut note = crate::item::make_item(
            &state,
            &engine.config,
            "keep this Constraint about the public API".into(),
            ContextKind::Constraint,
            ContextScope::Session,
            ContextRetention::Working,
            0.9,
            None,
        );
        note.scope_id = None;
        let id = log.id;
        let heap_body = log.content.clone();
        state.items.push(log);
        state.items.push(note);
        (id, heap_body)
    };
    let without = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 200,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !without.items.iter().any(|item| item.item_id == log_id),
        "full identity-log stdout must not fit a 200-token budget"
    );
    let with = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 200,
            hints: ContextHints {
                checked_files: vec!["src/big.rs@aaa".into()],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let selected = with
        .items
        .iter()
        .find(|item| item.item_id == log_id)
        .expect("descriptor-priced identity log must fit");
    assert_eq!(selected.content, "src/big.rs@aaa");
    assert!(
        with.items
            .iter()
            .any(|item| item.kind == ContextKind::Constraint),
        "descriptor pricing must leave room for the constraint"
    );
    let state = engine.state.lock().await;
    let heap = state
        .items
        .iter()
        .find(|item| item.id == log_id)
        .expect("heap row");
    assert_eq!(heap.content, heap_body, "heap body stays intact");
}

use std::sync::Arc;

use agent_contracts::{
    AgentError, AttentionState, BoundedCompactor, CompactionOutput, CompactionReason,
    CompactionRequest, ContextAction, ContextEngine, ContextHints, ContextIngress, ContextItemId,
    ContextKind, ContextMaintenanceTrigger, ContextQuery, ContextResidency, ContextRetention,
    ContextScope, ContextSearchQuery, DependencyKind, LifecycleLabel, ScopeId, ScopeKind,
    ScopeState, SemanticState, TaskId, ToolOutput,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

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
                facts: None,
                output: super::harness::observation_touching(
                    &format!("step-{i}"),
                    true,
                    content,
                    Some("AuthService.rs"),
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
            kind: Some(ContextKind::ToolObservation),
            scope: None,
            task_id: None,
            label: None,
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
    let fetched = engine
        .fetch_external(target)
        .await
        .unwrap()
        .expect("admitted Resident fetch returns the catalog body");
    assert_eq!(fetched.id, target);
    assert_eq!(
        fetched.residency,
        agent_contracts::ContextResidency::Resident
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
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching(
                "1",
                true,
                &format!("step 0: fix AuthService.rs {}", "z".repeat(200)),
                Some("AuthService.rs"),
            ),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching(
                "2",
                true,
                &format!("step 1: fix CacheStore.rs {}", "z".repeat(200)),
                Some("CacheStore.rs"),
            ),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    let refs = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "AuthService".into(),
            kind: Some(ContextKind::ToolObservation),
            scope: None,
            task_id: None,
            label: None,
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

/// admit 的 store 读取是一次回滚边界：当 blob 在 plan 与 IO 之间消失
/// （崩溃、手动删除），admit 必须是静默 no-op —— 条目留在外部映射、
/// 不产生任何新项、也没有挂起的转换。半截 admit 永远不允许存在。
#[tokio::test]
async fn admit_of_a_disappeared_store_blob_is_a_silent_no_op() {
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
    let content = format!("step 0: fix AuthService.rs {}", "x".repeat(160));
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching("step-0", true, &content, Some("AuthService.rs")),
            scope_id: None,
        })
        .await
        .unwrap();
    let content = format!("step 1: fix AuthService.rs {}", "y".repeat(160));
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching("step-1", true, &content, Some("AuthService.rs")),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc_report = engine.gc().await.unwrap();
    assert!(
        gc_report.externalized >= 1,
        "buffer overflow must externalize"
    );

    let target = {
        let state = engine.state.lock().await;
        state
            .external
            .iter()
            .find(|entry| entry.residency == ContextResidency::Cold)
            .map(|entry| entry.item_id)
            .expect("gc must leave a Cold store entry")
    };
    assert!(
        engine.fetch_external(target).await.unwrap().is_some(),
        "the blob must be readable before removal"
    );

    // 删除 blob 文件：plan_admit 只看映射（仍判定可检索），锁外读返回
    // None —— 必须静默 no-op，不留下任何半截状态。
    let store = crate::store::store_dir(&SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    std::fs::remove_file(store.join(format!("{target}.json"))).unwrap();

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: target,
                reason: "need it again".into(),
            },
        })
        .await
        .unwrap();

    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(target).is_some(),
            "the entry must stay in the external map"
        );
        assert!(
            !state.items.iter().any(|item| item.id == target),
            "no half-admitted resident may exist"
        );
        assert!(
            state
                .pending_ingest_transitions
                .iter()
                .all(|t| t.item_id != target),
            "no pending admit transition may exist"
        );
    }
    // materialize 也不应选中它（内容仍在外部层）。
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "next".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(!materialized.items.iter().any(|item| item.item_id == target));
}

/// 持久保留内容经 admit 后保持其保留类别：指令只移动 body 位置
/// （external -> resident），绝不改变生命周期权威（retention 保持
/// Durable）。
#[tokio::test]
async fn admit_of_a_durable_item_keeps_its_retention() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // 直接构造一个 Durable 的外部条目（持久根不会被 GC 驱逐，因此通过
    // restore 种入）。
    let mut state = crate::engine::State::default();
    let config = SimpleContextConfig::default();
    let mut item = crate::item::make_item(
        &state,
        &config,
        "durable decision: keep the module split".into(),
        ContextKind::Note,
        ContextScope::Task,
        ContextRetention::Durable,
        0.6,
        Some("seeded".to_string()),
    );
    item.id = ContextItemId::new();
    let reference = crate::store::externalize(dir.path(), &item).unwrap();
    state.external.push(crate::store::to_external_entry(
        &item, reference, 1, 1, None,
    ));
    let value = crate::checkpoint::serialize(&state).unwrap();
    engine.restore(value).await.unwrap();

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: item.id,
                reason: "the decision is relevant again".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let resident = state
        .items
        .iter()
        .find(|i| i.id == item.id)
        .expect("the durable item is resident after admit");
    assert_eq!(
        resident.retention,
        ContextRetention::Durable,
        "retention must not change when the body moves"
    );
    assert_eq!(resident.residency, ContextResidency::Resident);
    assert!(
        state.external.get(item.id).is_none(),
        "the entry must leave the external map"
    );
}

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
            kind: Some(ContextKind::ToolObservation),
            scope: None,
            task_id: None,
            label: None,
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
/// reconcile, materialization, state-changing retrieval, ingress,
/// maintenance, scope mutation, ledger export, checkpoint or restore must
/// block. Releasing the gate lets every one of them run to completion.
/// Without the gate a restore could
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
    let materialize = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .materialize(ContextQuery {
                    current_input: "operation gate regression".into(),
                    budget_tokens: 1_000,
                    hints: ContextHints::default(),
                })
                .await
        })
    };
    let admit = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .ingest(ContextIngress::ContextDirective {
                    action: ContextAction::Admit {
                        item_id: ContextItemId::new(),
                        reason: "operation gate regression".into(),
                    },
                })
                .await
        })
    };
    let ingest = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: "operation gate regression".into(),
                })
                .await
        })
    };
    let maintain = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.maintain(ContextMaintenanceTrigger::AfterModel).await })
    };
    let fetch = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.fetch_external(ContextItemId::new()).await })
    };
    let search = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .search_external(ContextSearchQuery::new(String::new(), 1))
                .await
        })
    };
    let inspect = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.inspect_external(ContextItemId::new()).await })
    };
    let open_scope = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.open_scope(ScopeKind::Tool, None).await })
    };
    let close_scope = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.close_scope(ScopeId::new()).await })
    };
    let export = {
        let engine = Arc::clone(&engine);
        let path = dir.path().join("operation-gate-ledger.jsonl");
        tokio::spawn(async move { engine.export_ledger(&path).await })
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

    // None may complete while the gate is held.
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
        !materialize.is_finished(),
        "materialize must wait for the operation gate"
    );
    assert!(
        !admit.is_finished(),
        "Admit must wait for the operation gate"
    );
    assert!(
        !ingest.is_finished(),
        "ingest must wait for the operation gate"
    );
    assert!(
        !maintain.is_finished(),
        "maintenance must wait for the operation gate"
    );
    assert!(
        !fetch.is_finished(),
        "Fetch must wait for the operation gate"
    );
    assert!(
        !search.is_finished(),
        "search must wait for the operation gate"
    );
    assert!(
        !inspect.is_finished(),
        "state-changing inspect must wait for the operation gate"
    );
    assert!(
        !open_scope.is_finished(),
        "scope open must wait for the operation gate"
    );
    assert!(
        !close_scope.is_finished(),
        "scope close must wait for the operation gate"
    );
    assert!(
        !export.is_finished(),
        "ledger export must wait for the operation gate"
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
    materialize.await.unwrap().unwrap();
    admit.await.unwrap().unwrap();
    ingest.await.unwrap().unwrap();
    maintain.await.unwrap().unwrap();
    fetch.await.unwrap().unwrap();
    search.await.unwrap().unwrap();
    inspect.await.unwrap().unwrap();
    open_scope.await.unwrap().unwrap();
    close_scope.await.unwrap().unwrap();
    export.await.unwrap().unwrap();
    checkpoint.await.unwrap().unwrap();
    restore.await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_gc_and_external_admit_keep_exactly_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }));
    let item_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "ConcurrentOwner.rs durable outcome".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.8,
            None,
        );
        item.entities = crate::index::entity::extract_entities(&item.content);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        item.id
    };

    let gate = engine.op_gate.lock().await;
    let admit = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            engine
                .ingest(ContextIngress::ContextDirective {
                    action: ContextAction::Admit {
                        item_id,
                        reason: "concurrent owner regression".into(),
                    },
                })
                .await
        })
    };
    let gc = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.gc().await })
    };
    tokio::task::yield_now().await;
    assert!(!admit.is_finished() && !gc.is_finished());
    drop(gate);
    admit.await.unwrap().unwrap();
    gc.await.unwrap().unwrap();

    let state = engine.state.lock().await;
    let owners = usize::from(state.items.iter().any(|item| item.id == item_id))
        + usize::from(state.eviction_buffer.iter().any(|item| item.id == item_id))
        + usize::from(state.external.get(item_id).is_some());
    assert_eq!(
        owners, 1,
        "GC recall and Admit must never duplicate or lose one context identity"
    );
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

    // 同一 id 同时被堆和外部映射持有（跨驻留层所有权冲突）。
    let mut heap_external = crate::engine::State::default();
    let id = ContextItemId::new();
    let item = make_item(&heap_external, id, "heap + external body");
    heap_external.items.replace_all(vec![item.clone()]);
    heap_external.external.push(crate::store::to_external_entry(
        &item,
        crate::store::make_context_ref(&item),
        1,
        1,
        None,
    ));
    expect_restore_rejected(&heap_external, "owned by both").await;

    // 同一 id 同时被可逆缓冲区和外部映射持有。
    let mut buffer_external = crate::engine::State::default();
    let id = ContextItemId::new();
    let item = make_item(&buffer_external, id, "buffer + external body");
    buffer_external.eviction_buffer.push(item.clone());
    buffer_external
        .external
        .push(crate::store::to_external_entry(
            &item,
            crate::store::make_context_ref(&item),
            1,
            1,
            None,
        ));
    expect_restore_rejected(&buffer_external, "owned by both").await;

    // 外部映射内部出现重复 id（映射查找会掩盖它，只有原始扫描能发现）。
    let mut dup_external = crate::engine::State::default();
    let id = ContextItemId::new();
    let item = make_item(&dup_external, id, "duplicated external body");
    let entry =
        crate::store::to_external_entry(&item, crate::store::make_context_ref(&item), 1, 1, None);
    dup_external.external.push(entry.clone());
    dup_external.external.push(entry);
    expect_restore_rejected(&dup_external, "owned by both").await;

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

/// A refused restore must leave the running state untouched: structural
/// validation runs on a scratch copy before any replacement, so a corrupt
/// or hostile checkpoint cannot clobber live items. Regression: the live
/// state was committed first and stayed clobbered after the validation
/// error. A blob that fails to deserialize at all is refused the same way,
/// before anything is committed.
#[tokio::test]
async fn rejected_restore_leaves_the_running_state_intact() {
    let config = SimpleContextConfig::default();
    let engine = SimpleContextEngine::new(config.clone());
    let body = "live observation that must survive a refused restore";

    // A self-consistent live state: one resident item plus its catalog.
    let live_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &config,
            body.into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.id = ContextItemId::new();
        let id = item.id;
        state.items.replace_all(vec![item]);
        state.sync_catalog();
        id
    };

    // A checkpoint that deserializes but violates the structural
    // invariants (one id held twice by the heap). Restore must refuse it
    // and keep the live heap, catalog and scope tree untouched.
    let duplicate_id_blob = {
        let mut corrupt = crate::engine::State::default();
        let mut dup = crate::item::make_item(
            &corrupt,
            &config,
            "corrupt duplicate body".into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            None,
        );
        dup.id = ContextItemId::new();
        corrupt.items.replace_all(vec![dup.clone(), dup]);
        crate::checkpoint::serialize(&corrupt).unwrap()
    };
    let error = engine
        .restore(duplicate_id_blob)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("checkpoint restore validation") && error.contains("more than once"),
        "expected the duplicate-id validation error, got: {error}"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.items.len(),
            1,
            "the refused restore replaced the live heap"
        );
        assert_eq!(state.items[0].content, body);
        assert!(
            state.catalog.contains(live_id),
            "the refused restore dropped the live id from the catalog"
        );
    }

    // A blob that cannot deserialize at all (null is not a state) fails
    // before anything is committed, with the live state still in place.
    let error = engine
        .restore(serde_json::Value::Null)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("checkpoint restore"),
        "expected a restore error, got: {error}"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.items.len(),
            1,
            "the failed deserialize replaced the live heap"
        );
        assert_eq!(state.items[0].content, body);
    }
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
        root.dependencies
            .push(agent_contracts::DependencyEdge::continuation(evidence_id));
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

#[tokio::test]
async fn derived_from_does_not_reactivate_compacted_sources() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let source_id = {
        let mut state = engine.state.lock().await;
        let mut source = crate::item::make_item(
            &state,
            &engine.config,
            "raw episode body that compaction already folded".into(),
            ContextKind::UserMessage,
            ContextScope::Task,
            ContextRetention::Working,
            0.1,
            None,
        );
        let source_id = source.id;
        source.attention = AttentionState::Archived;
        source.residency = ContextResidency::Warm;
        source.evicted_at_tick = Some(0);
        state.eviction_buffer.push(source);

        let mut summary = crate::item::make_item(
            &state,
            &engine.config,
            "compact episode card".into(),
            ContextKind::Summary,
            ContextScope::Pinned,
            ContextRetention::Pinned,
            0.9,
            None,
        );
        summary
            .dependencies
            .push(agent_contracts::DependencyEdge::derived_from(source_id));
        state.items.replace_all(vec![summary]);
        source_id
    };

    let report = engine.gc().await.unwrap();
    assert!(
        !report
            .reactivations
            .iter()
            .any(|r| r.item_id == source_id && r.reason.contains("dependency of a marked root")),
        "DerivedFrom is provenance, not a residency root: {:?}",
        report.reactivations
    );
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|item| item.id == source_id),
            "compacted sources must not return to Resident through DerivedFrom"
        );
        assert!(
            state
                .eviction_buffer
                .iter()
                .any(|item| item.id == source_id),
            "the source stays Warm"
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

/// A named task summary belongs to the *completed* task, never to whatever
/// scope is active when the completion arrives: when task A completes while
/// task B is focused, the summary must carry A's task id and scope, and B's
/// focus must stay untouched.
#[tokio::test]
async fn named_task_summary_does_not_inherit_the_current_focus() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_a = open_focus(&engine, "finish task A").await;
    // Switch focus to task B: task A's scope suspends, B's opens.
    let task_b = open_focus(&engine, "work on task B").await;

    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_a),
            summary: "A is done".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    assert_eq!(
        state.focus.as_ref().map(|f| f.task_id),
        Some(task_b),
        "completing an unrelated task must not clear the focused task"
    );
    let summary = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::Summary)
        .expect("a summary item must exist");
    assert_eq!(
        summary.task_id,
        Some(task_a),
        "the summary must belong to the completed task, not the focused one"
    );
    let task_a_scope = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_a))
        .map(|scope| scope.id)
        .expect("task A must have a scope");
    assert_eq!(
        summary.scope_id,
        Some(task_a_scope),
        "the summary must point at the completed task's scope"
    );
}

/// An unnamed task completion (the runtime's focus is the completed task)
/// still stamps the summary with the focused task's identity — the summary
/// must not lose its task/scope stamp because the focus is cleared while
/// the item is built.
#[tokio::test]
async fn unnamed_task_summary_keeps_the_focused_tasks_identity() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_a = open_focus(&engine, "finish task A").await;

    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: None,
            summary: "A is done".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    assert!(
        state.focus.is_none(),
        "completing the focused task must clear the focus"
    );
    let summary = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::Summary)
        .expect("a summary item must exist");
    assert_eq!(
        summary.task_id,
        Some(task_a),
        "the summary must keep the completed task's id even though the \
         focus was cleared while it was built"
    );
    let task_a_scope = state
        .scopes
        .iter()
        .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_a))
        .map(|scope| scope.id)
        .expect("task A must have a scope");
    assert_eq!(
        summary.scope_id,
        Some(task_a_scope),
        "the summary must point at the completed task's scope"
    );
    assert_eq!(
        summary.content, "A is done",
        "without a compactor the runtime summary is stored verbatim"
    );
    assert_eq!(summary.source.as_deref(), Some("task-summary"));
    assert!(
        summary.dependencies.is_empty(),
        "verbatim summaries have no DerivedFrom edges"
    );
}

/// A compactor on TaskCompleted must not spend a second LLM round: the
/// runtime summary is already the authoritative CompletionRecord text.
#[tokio::test]
async fn task_completion_stores_the_completion_record_summary_without_llm() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default())
        .with_compactor(Arc::new(TaskDistillCompactor));
    let task = open_focus(&engine, "finish task A").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "keep AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task),
            summary: "A is done".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let summary = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::Summary)
        .expect("completion summary must exist");
    assert_eq!(summary.task_id, Some(task));
    assert_eq!(summary.source.as_deref(), Some("task-summary"));
    assert_eq!(
        summary.content, "A is done",
        "CompletionRecord.summary is stored verbatim even when a compactor is wired"
    );
    assert!(
        summary.dependencies.is_empty(),
        "task completion is not an LLM-derived card"
    );
    drop(state);

    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.compaction_input_tokens, 0,
        "TaskCompleted must not call the compactor"
    );
    assert_eq!(diagnostics.compaction_output_tokens, 0);
}

/// Episode rotation still uses the LLM compactor; TaskCompleted does not.
struct TaskDistillCompactor;

#[async_trait::async_trait]
impl BoundedCompactor for TaskDistillCompactor {
    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> agent_contracts::AgentResult<CompactionOutput> {
        Ok(CompactionOutput {
            text: format!("[distilled] {}", request.source),
            input_tokens: 5,
            output_tokens: 3,
        })
    }
}

fn episode_budget_config() -> SimpleContextConfig {
    SimpleContextConfig {
        // Related messages never fire the semantic signal; only the turn
        // budget rotates. FocusChanged already bumps generation to 1, so
        // the second user message observes generation >= 2 and rotates.
        episode_rotate_threshold: 0.0,
        episode_max_user_turns: 2,
        ..SimpleContextConfig::default()
    }
}

async fn ingest_related(engine: &SimpleContextEngine, turn: u64) {
    engine
        .ingest(ContextIngress::UserMessage {
            content: format!("keep AuthService.rs in the login path round {turn}"),
        })
        .await
        .unwrap();
}

async fn ingest_semantic_outcome(engine: &SimpleContextEngine) {
    let mut state = engine.state.lock().await;
    let mut item = crate::item::make_item(
        &state,
        &engine.config,
        "keep the unversioned ping wire".into(),
        ContextKind::Constraint,
        ContextScope::Task,
        ContextRetention::Durable,
        0.9,
        Some("decision".into()),
    );
    item.tags.push(agent_contracts::label::Label::core(
        agent_contracts::label::CoreLabel::Constraint,
    ));
    state.items.push(item);
}

/// Episode rotation with a compactor distills the closing episode into a
/// derived Summary; ordinary dialogue is archived, not destroyed.
#[tokio::test]
async fn episode_rotation_distills_with_derived_from_and_keeps_sources() {
    let engine = SimpleContextEngine::new(episode_budget_config())
        .with_compactor(Arc::new(TaskDistillCompactor));
    let task = open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_semantic_outcome(&engine).await;
    ingest_related(&engine, 2).await;

    let state = engine.state.lock().await;
    let source = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::UserMessage && item.content.contains("round 1"))
        .expect("first-episode user message must remain retrievable");
    let source_id = source.id;
    assert_eq!(
        source.attention,
        AttentionState::Archived,
        "ordinary dialogue leaves the working set on rotation"
    );
    let summary = state
        .items
        .iter()
        .find(|item| {
            item.kind == ContextKind::Summary && item.source.as_deref() == Some("episode-derived")
        })
        .expect("episode distill summary must exist");
    assert_eq!(summary.task_id, Some(task));
    assert_eq!(summary.retention, ContextRetention::Durable);
    assert!(
        summary.content.contains("[distilled]"),
        "compactor output must become the episode card, got: {}",
        summary.content
    );
    assert!(
        summary.content.contains("round 1"),
        "distill source must include the closing episode body, got: {}",
        summary.content
    );
    assert!(
        !summary.content.contains("round 2"),
        "the new episode's message must not be in the closing-episode card, got: {}",
        summary.content
    );
    assert!(
        summary
            .dependencies
            .iter()
            .any(|edge| edge.kind == DependencyKind::DerivedFrom && edge.target == source_id),
        "episode card must carry DerivedFrom, got: {:?}",
        summary.dependencies
    );
    drop(state);

    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.compaction_input_tokens, 5);
    assert_eq!(diagnostics.compaction_output_tokens, 3);

    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    assert_eq!(report.compactions.len(), 1);
    assert_eq!(
        report.compactions[0].reason,
        CompactionReason::EpisodeRotation
    );
    assert_eq!(report.compaction_input_tokens, 5);
    assert_eq!(report.compaction_output_tokens, 3);
}

/// A later rotation supersedes the previous episode card; the newest card
/// stays live. Raw bodies of the older episode remain.
#[tokio::test]
async fn later_episode_card_supersedes_the_previous_one() {
    let engine = SimpleContextEngine::new(episode_budget_config())
        .with_compactor(Arc::new(TaskDistillCompactor));
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_semantic_outcome(&engine).await;
    ingest_related(&engine, 2).await;
    let first_card = engine
        .state
        .lock()
        .await
        .items
        .iter()
        .find(|item| item.source.as_deref() == Some("episode-derived"))
        .map(|item| item.id)
        .expect("first episode card");

    ingest_related(&engine, 3).await;
    ingest_semantic_outcome(&engine).await;
    ingest_related(&engine, 4).await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let first = state
        .items
        .iter()
        .find(|item| item.id == first_card)
        .expect("superseded card stays addressable");
    assert!(
        matches!(first.semantic, SemanticState::Superseded { .. }),
        "prior episode card must be superseded, got {:?}",
        first.semantic
    );
    let live_cards: Vec<_> = state
        .items
        .iter()
        .filter(|item| item.source.as_deref() == Some("episode-derived") && item.semantic.is_live())
        .collect();
    assert_eq!(
        live_cards.len(),
        1,
        "at most one live episode card per task, got {live_cards:?}"
    );
    assert!(
        live_cards[0].content.contains("round 2") || live_cards[0].content.contains("round 3"),
        "the live card must distill the second episode, got: {}",
        live_cards[0].content
    );
    assert!(
        state.items.iter().any(|item| {
            item.kind == ContextKind::UserMessage && item.content.contains("round 1")
        }),
        "raw first-episode body stays retrievable"
    );
}

/// Compact failure must not fail the user turn: rotation still lands, and
/// the card falls back to a bounded marker.
#[tokio::test]
async fn episode_rotation_compact_failure_falls_back_and_does_not_fail_ingest() {
    struct FailingCompactor;
    #[async_trait::async_trait]
    impl BoundedCompactor for FailingCompactor {
        async fn compact(
            &self,
            _request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            Err(AgentError::Model("boom".into()))
        }
    }

    let engine = SimpleContextEngine::new(episode_budget_config())
        .with_compactor(Arc::new(FailingCompactor));
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_semantic_outcome(&engine).await;
    ingest_related(&engine, 2).await;

    let state = engine.state.lock().await;
    let summary = state
        .items
        .iter()
        .find(|item| item.source.as_deref() == Some("episode-derived"))
        .expect("fallback episode card must exist");
    assert!(
        summary.content.contains("[episode]"),
        "compact failure must fall back to the bounded marker, got: {}",
        summary.content
    );
    assert!(
        summary.content.contains("round 1"),
        "fallback still carries bounded episode source, got: {}",
        summary.content
    );
}

/// Without a compactor, rotation stays promote-and-evict: no derived card.
#[tokio::test]
async fn episode_rotation_without_compactor_does_not_insert_a_card() {
    let engine = SimpleContextEngine::new(episode_budget_config());
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_related(&engine, 2).await;
    let state = engine.state.lock().await;
    assert!(
        !state
            .items
            .iter()
            .any(|item| item.source.as_deref() == Some("episode-derived")),
        "no episode card without a compactor"
    );
}

#[tokio::test]
async fn short_episode_without_semantic_outcome_skips_llm_compactor() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingCompactor(AtomicUsize);
    #[async_trait::async_trait]
    impl BoundedCompactor for CountingCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionOutput {
                text: format!("[distilled] {}", request.source),
                input_tokens: 9,
                output_tokens: 3,
            })
        }
    }

    let counter = Arc::new(CountingCompactor(AtomicUsize::new(0)));
    let engine = SimpleContextEngine::new(episode_budget_config()).with_compactor(counter.clone());
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_related(&engine, 2).await;
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.compaction_input_tokens, 0);
    let state = engine.state.lock().await;
    assert!(
        !state
            .items
            .iter()
            .any(|item| item.source.as_deref() == Some("episode-derived")),
        "short chatter must not mint an LLM episode card"
    );
}

#[tokio::test]
async fn long_episode_without_semantic_delta_rotates_without_paying_llm() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingCompactor(AtomicUsize);
    #[async_trait::async_trait]
    impl BoundedCompactor for CountingCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionOutput {
                text: format!("[distilled] {}", request.source),
                input_tokens: 9,
                output_tokens: 3,
            })
        }
    }

    let counter = Arc::new(CountingCompactor(AtomicUsize::new(0)));
    let config = SimpleContextConfig {
        episode_rotate_threshold: 0.0,
        episode_max_user_turns: 5,
        ..SimpleContextConfig::default()
    };
    let engine = SimpleContextEngine::new(config).with_compactor(counter.clone());
    open_focus(&engine, "keep AuthService.rs").await;
    for turn in 1..=5 {
        ingest_related(&engine, turn).await;
    }
    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .any(|row| row.reason.contains("episode rotated")),
        "the turn budget must still rotate a long related episode"
    );
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        0,
        "generation >= 4 must not auto-pay the LLM distiller without a semantic delta"
    );
}

#[tokio::test]
async fn late_constraint_survives_episode_rotation() {
    let engine = SimpleContextEngine::new(episode_budget_config())
        .with_compactor(Arc::new(TaskDistillCompactor));
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_related(&engine, 2).await;
    ingest_semantic_outcome(&engine).await;
    ingest_related(&engine, 3).await;
    let state = engine.state.lock().await;
    assert!(
        state.items.iter().any(|item| {
            item.kind == ContextKind::Constraint
                && item.content.contains("unversioned ping")
                && item.semantic.is_live()
        }),
        "a late durable constraint must survive the next episode rotation"
    );
}

#[tokio::test]
async fn restore_zeros_segment_local_reactivation_counters() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    {
        let mut state = engine.state.lock().await;
        state.reactivation_events = 9;
        state.unique_reactivated = 4;
        state.reactivation_selected = 3;
    }
    let blob = engine.checkpoint().await.unwrap();
    engine.restore(blob).await.unwrap();
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.reactivation_events, 0);
    assert_eq!(diagnostics.unique_reactivated, 0);
    assert_eq!(diagnostics.reactivation_selected, 0);
}

#[tokio::test]
async fn file_observation_alone_does_not_pay_for_episode_compaction() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingCompactor(AtomicUsize);
    #[async_trait::async_trait]
    impl BoundedCompactor for CountingCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionOutput {
                text: format!("[distilled] {}", request.source),
                input_tokens: 9,
                output_tokens: 3,
            })
        }
    }

    let counter = Arc::new(CountingCompactor(AtomicUsize::new(0)));
    let engine = SimpleContextEngine::new(episode_budget_config()).with_compactor(counter.clone());
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "src/auth.rs observed".into(),
            ContextKind::FileObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.7,
            Some("fs.read".into()),
        );
        item.file_path = Some("src/auth.rs".into());
        state.items.push(item);
    }
    ingest_related(&engine, 2).await;
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn force_episode_llm_distill_pays_without_semantic_delta() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingCompactor(AtomicUsize);
    #[async_trait::async_trait]
    impl BoundedCompactor for CountingCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CompactionOutput {
                text: format!("[distilled] {}", request.source),
                input_tokens: 9,
                output_tokens: 3,
            })
        }
    }

    let counter = Arc::new(CountingCompactor(AtomicUsize::new(0)));
    let mut config = episode_budget_config();
    config.force_episode_llm_distill = true;
    let engine = SimpleContextEngine::new(config).with_compactor(counter.clone());
    open_focus(&engine, "keep AuthService.rs").await;
    ingest_related(&engine, 1).await;
    ingest_related(&engine, 2).await;
    assert!(
        counter.0.load(Ordering::SeqCst) >= 1,
        "force-compact must pay for an episode card even without a semantic delta"
    );
}

/// The dependency-expansion token reserve is only carved out when expansion
/// can actually run: with expansion disabled the whole budget belongs to the
/// working set, so an item that fits the budget must not be pushed out by a
/// reserve that is never spent.
#[tokio::test]
async fn dependency_reserve_is_not_taken_when_expansion_is_disabled() {
    // ~900 tokens of content: fits an 900-token budget only when the
    // expansion reserve (min(1024, budget) = 900) is not carved out.
    let content = "x".repeat(3_600);

    let off = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    {
        let mut state = off.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &off.config,
            content.clone(),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.scope_id = None;
        state.items.push(item);
    }
    let with_off = off
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 900,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !with_off.items.is_empty(),
        "with expansion disabled the full budget must be spendable on the \
         working set, got {} items",
        with_off.items.len()
    );

    let on = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: true,
        ..SimpleContextConfig::default()
    });
    {
        let mut state = on.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &on.config,
            content,
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.5,
            None,
        );
        item.scope_id = None;
        // Reserve is taken only when a candidate actually has a Continuation
        // edge; the flag alone does not tax frames with no such edge.
        item.dependencies
            .push(agent_contracts::DependencyEdge::continuation(
                ContextItemId::new(),
            ));
        state.items.push(item);
    }
    let with_on = on
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 900,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        with_on.items.is_empty(),
        "with expansion enabled the reserve must be carved out first, so an \
         item exactly at the budget no longer fits"
    );
}

/// Fit packing must not hide a lower-ranked item that fits behind an
/// oversized top item: candidates are scored first, but the working set is
/// packed afterwards, so an item too big for the remaining budget must not
/// consume the cap and bury everything below it.
#[tokio::test]
async fn oversized_top_item_does_not_hide_a_lower_ranked_item_that_fits() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        dependency_expansion: false,
        ..SimpleContextConfig::default()
    });
    let (big_id, small_id) = {
        let mut state = engine.state.lock().await;
        // High-importance (top-ranked) but ~2000 tokens: cannot fit a
        // 150-token budget.
        let mut big = crate::item::make_item(
            &state,
            &engine.config,
            "y".repeat(8_000),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            1.0,
            None,
        );
        big.scope_id = None;
        big.importance = 1.0;
        let big_id = big.id;
        state.items.push(big);
        // Low-importance but ~100 tokens: fits the same budget.
        let mut small = crate::item::make_item(
            &state,
            &engine.config,
            "z".repeat(400),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.1,
            None,
        );
        small.scope_id = None;
        small.importance = 0.1;
        let small_id = small.id;
        state.items.push(small);
        (big_id, small_id)
    };

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 150,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.item_id == small_id),
        "the item that fits must be selected even though the top-ranked item \
         is too big for the budget"
    );
    assert!(
        !materialized.items.iter().any(|item| item.item_id == big_id),
        "the oversized top item must not be forced into the frame"
    );
}

/// External refs are token-capped (not just count-capped) and their token
/// cost is charged against the snapshot: long summaries stop the ranked
/// walk early, and `approx_tokens` reflects the refs the model sees.
#[tokio::test]
async fn external_refs_are_token_capped_and_charged() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let long_id = {
        let mut state = engine.state.lock().await;
        // Two entries whose summaries alone would exceed the 512-token
        // external-ref bound: the ranked walk must stop after the first.
        let mut first = crate::item::make_item(
            &state,
            &engine.config,
            "first external body".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Durable,
            0.9,
            None,
        );
        first.scope_id = None;
        let first_id = first.id;
        let mut second = crate::item::make_item(
            &state,
            &engine.config,
            "second external body".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Durable,
            0.9,
            None,
        );
        second.scope_id = None;
        let second_id = second.id;
        let reference = |id: ContextItemId, summary: String| agent_contracts::ContextRef {
            uri: format!("context://run/{id}"),
            item_id: id,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            summary,
            created_tick: 0,
        };
        // ~300 tokens per summary; two of them exceed the 512 bound. The
        // entries rank by recency, so the more recent one (tick 2) is
        // walked first and must survive the token cap.
        let long_summary = "s".repeat(1_200);
        state.external.push(crate::store::to_external_entry(
            &first,
            reference(first_id, long_summary.clone()),
            2,
            1,
            None,
        ));
        state.external.push(crate::store::to_external_entry(
            &second,
            reference(second_id, long_summary),
            1,
            1,
            None,
        ));
        first_id
    };

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        materialized.external.len(),
        1,
        "long summaries must stop the ranked ref walk at the token bound, \
         got {} refs",
        materialized.external.len()
    );
    assert_eq!(
        materialized.external.as_slice()[0].item_id,
        long_id,
        "the first-ranked ref must survive the token cap"
    );
    let external_tokens: usize = materialized
        .external
        .iter()
        .map(|entry| {
            crate::item::approx_tokens(&entry.context_ref.uri)
                + crate::item::approx_tokens(&entry.context_ref.summary)
        })
        .sum();
    assert!(
        materialized.approx_tokens >= external_tokens,
        "the snapshot's token total must include the external refs' cost"
    );
}

/// Candidate generation and scoring share one matching universe: the exact
/// entity index cannot express a substring overlap (`src/auth/AuthService.rs`
/// vs a hot `AuthService.rs`), so the residual pass must bring the item into
/// the candidate set or the scorer's substring affinity can never fire for
/// it.
#[tokio::test]
async fn substring_entity_match_reaches_the_candidate_universe() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let closed_scope_id = {
        let mut state = engine.state.lock().await;
        let session_id = crate::scope::ensure_session(&mut state);
        let closed = state.scopes.push(agent_contracts::Scope {
            id: ScopeId::new(),
            parent: Some(session_id),
            kind: ScopeKind::Focus,
            state: ScopeState::Closed,
            task_id: None,
            goal: None,
            opened_tick: 1,
            last_active_tick: 1,
            closed_tick: Some(2),
        });
        // An item in a closed scope with an entity that only overlaps the
        // hot entity as a substring: the exact index misses it, the scorer
        // would match it — the residual pass must close the gap.
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "auth work on the service".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.8,
            None,
        );
        item.scope_id = Some(closed);
        item.entities = vec!["src/auth/AuthService.rs".into()];
        state.items.push(item);
        closed
    };
    // A hot entity naming the substring overlap.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs now".into(),
        })
        .await
        .unwrap();
    let _ = closed_scope_id;

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "work on AuthService.rs now".into(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.content.contains("auth work on the service")),
        "a substring entity match must reach the candidate universe and be \
         selected, exactly like the scorer's affinity promises"
    );
}

#[tokio::test]
async fn lifecycle_ledger_records_maintenance_and_gc_rows() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
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
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "step 1: fix AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    // AfterModel consumes the observation (attention row); the full GC
    // evicts it (gc row) — both must land in the artifact-backed ledger.
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    let path = dir.path().join("lifecycle.jsonl");
    let count = engine.export_ledger(&path).await.unwrap();
    assert!(
        count >= 2,
        "maintain + gc must produce ledger rows, got {count}"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"axis\":\"attention\"") && text.contains("\"axis\":\"gc\""),
        "the artifact must carry attention and gc rows: {text}"
    );
    assert!(
        text.contains("\"trigger\":\"maintain\"") || text.contains("\"trigger\":\"AfterModel\""),
        "the row names its trigger: {text}"
    );
    assert!(
        text.contains("\"turn\":")
            && text.contains("\"event_seq\":")
            && text.contains("\"revision\":"),
        "the row carries turn, event sequence and per-item revision: {text}"
    );
    // Export is explicit and drains the buffer: a second export is empty.
    assert_eq!(
        engine.export_ledger(&path).await.unwrap(),
        0,
        "export clears the in-engine buffer"
    );
}

#[tokio::test]
async fn lifecycle_ledger_survives_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
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
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "step 1: fix AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    // AfterModel consumes the observation: an attention row lands in the
    // ledger before the checkpoint.
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let checkpoint = engine.checkpoint().await.unwrap();
    let restored = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    restored.restore(checkpoint).await.unwrap();

    let path = dir.path().join("lifecycle-restored.jsonl");
    let count = restored.export_ledger(&path).await.unwrap();
    assert!(
        count >= 1,
        "the restored engine must keep the ledger across the checkpoint, got {count}"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("\"cause\":\"ephemeral") || text.contains("\"axis\":\"attention\""),
        "the restored ledger keeps the cause and axis: {text}"
    );
}

#[tokio::test]
async fn materialize_preview_is_a_read_that_advances_no_clock() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    // The current-turn user message is TurnFrame-owned and skipped in the
    // historical working set. A second ingest makes the first message
    // historical so the preview has something to see.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "still looking at AuthService.rs".into(),
        })
        .await
        .unwrap();
    let before = engine.diagnostics().await.unwrap().event_seq;
    // Preview three times: a read must not advance the event sequence, so
    // merely looking at the context never ages TTLs or recency scores.
    for _ in 0..3 {
        let preview = engine
            .materialize(ContextQuery {
                current_input: "look".into(),
                budget_tokens: 8_192,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        assert!(!preview.items.is_empty(), "the preview sees the message");
    }
    let after = engine.diagnostics().await.unwrap().event_seq;
    assert_eq!(
        before, after,
        "materializing a preview is a read and must not advance the event sequence"
    );
    // A state change still advances it.
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
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
    let later = engine.diagnostics().await.unwrap().event_seq;
    assert!(later > after, "ingest advances the event sequence");
}

#[tokio::test]
async fn selection_stamp_is_written_only_by_consumption_ack() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let first = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == ContextKind::UserMessage)
        .expect("the first message is present");
    assert_eq!(
        first.last_selected_turn, first.created_turn,
        "an item is born selected in its own turn"
    );

    // A second user turn; its preview must not stamp the first message.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "keep going".into(),
        })
        .await
        .unwrap();
    let preview = engine
        .materialize(ContextQuery {
            current_input: "keep going".into(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let untouched = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == ContextKind::UserMessage && s.created_turn == first.created_turn)
        .expect("the first message survives");
    assert_eq!(
        untouched.last_selected_turn, first.created_turn,
        "a non-consuming preview must not stamp selection"
    );

    // Consumption stamps it with the turn the model actually saw it.
    acknowledge_all(&engine, &preview).await;
    let consumed = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == ContextKind::UserMessage && s.created_turn == first.created_turn)
        .expect("the first message survives the ack");
    assert_eq!(
        consumed.last_selected_turn, consumed.last_access_turn,
        "the ack stamps the selection and access turns together"
    );
    assert!(
        consumed.last_selected_turn > first.created_turn,
        "the item was selected in a later turn than it was born"
    );
}

#[tokio::test]
async fn ephemeral_ttl_counts_user_turns_not_events() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        turn_ttl_ticks: 2,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    // Turn 1: a consumed ephemeral observation.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "ok".into(),
                model_content: "step 1: fix AuthService.rs".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // A burst of events inside the same turn (maintain passes + previews)
    // must not age the TTL: it counts user turns, not event ticks.
    for _ in 0..10 {
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        engine
            .materialize(ContextQuery {
                current_input: "look".into(),
                budget_tokens: 8_192,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
    }
    let live = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == ContextKind::ToolObservation);
    assert_eq!(
        live.expect("the observation is in the catalog").semantic,
        SemanticState::Live,
        "same-turn events must not tombstone the TTL item"
    );

    // Two more user turns (created at turn 1, TTL 2): turn 3 is age 2, turn
    // 4 is age 3 > 2 and the lifecycle ends. The AfterModel branch never
    // reaches the TTL (a consumed ephemeral observation stays recallable
    // there); a non-model trigger runs the full residency machine.
    for _ in 0..3 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: "next topic please".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterTool)
            .await
            .unwrap();
    }
    let dead = engine
        .inspect(usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.kind == ContextKind::ToolObservation);
    assert_eq!(
        dead.expect("the observation is in the catalog").semantic,
        SemanticState::Tombstoned,
        "the ephemeral TTL expires after turn_ttl_ticks user turns, not events"
    );
}

/// The external ref view never walks the whole map: a hot-entity match from
/// the *head* of a large external map (long past the recency tail) must
/// still surface through the entity index, without materializing the rest.
#[tokio::test]
async fn external_view_surfaces_hot_matches_beyond_the_recency_tail() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let hot_id = {
        let mut state = engine.state.lock().await;
        let reference = |id: ContextItemId, summary: String| agent_contracts::ContextRef {
            uri: format!("context://run/{id}"),
            item_id: id,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            summary,
            created_tick: 0,
        };
        // One hot-entity entry at the head of the map...
        let mut hot = crate::item::make_item(
            &state,
            &engine.config,
            "hot stored decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Durable,
            0.9,
            None,
        );
        hot.scope_id = None;
        hot.entities = vec!["CacheStore.rs".into()];
        let hot_id = hot.id;
        state.external.push(crate::store::to_external_entry(
            &hot,
            reference(hot_id, "hot decision".into()),
            1,
            1,
            None,
        ));
        // ...followed by a thousand entries that push it far past the
        // recency tail.
        for i in 0..1_000 {
            let mut filler = crate::item::make_item(
                &state,
                &engine.config,
                format!("filler {i}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.2,
                None,
            );
            filler.scope_id = None;
            let filler_id = filler.id;
            state.external.push(crate::store::to_external_entry(
                &filler,
                reference(filler_id, format!("filler {i}")),
                i + 2,
                1,
                None,
            ));
        }
        hot_id
    };
    engine
        .ingest(ContextIngress::UserMessage {
            content: "check CacheStore.rs".into(),
        })
        .await
        .unwrap();

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "check CacheStore.rs".into(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .external
            .iter()
            .any(|entry| entry.item_id == hot_id),
        "a hot-entity match far beyond the recency tail must surface through \
         the entity index"
    );
}

/// A Checked file body that overflowed to the store still appears as a
/// `path@rev` descriptor even when it is not hot and sits far past the
/// recency tail. Lookup is the entity index (same O(bucket) as hot), and
/// ranking keeps Checked identity above later fillers so the 32-ref cap
/// does not hide it. The engine's `CheckedFiles` projection is enough;
/// this query does not re-name the file.
#[tokio::test]
async fn external_view_surfaces_checked_stored_file_beyond_the_recency_tail() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    const BODY: &str = "fn handle_secret() {}";
    let stored_id = {
        let mut state = engine.state.lock().await;
        let reference =
            |id: ContextItemId, summary: String, kind: ContextKind| agent_contracts::ContextRef {
                uri: format!("context://run/{id}"),
                item_id: id,
                kind,
                scope: ContextScope::Task,
                summary,
                created_tick: 0,
            };
        let mut stored = crate::item::make_item(
            &state,
            &engine.config,
            BODY.into(),
            ContextKind::ToolObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.9,
            None,
        );
        stored.scope_id = None;
        stored.entities = vec!["AuthService.rs".into()];
        stored.file_path = Some("AuthService.rs".into());
        stored.file_revision = Some("abc".into());
        let stored_id = stored.id;
        state.external.push(crate::store::to_external_entry(
            &stored,
            reference(stored_id, BODY.into(), ContextKind::ToolObservation),
            1,
            1,
            None,
        ));
        for i in 0..1_000 {
            let mut filler = crate::item::make_item(
                &state,
                &engine.config,
                format!("filler {i}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.2,
                None,
            );
            filler.scope_id = None;
            let filler_id = filler.id;
            state.external.push(crate::store::to_external_entry(
                &filler,
                reference(filler_id, format!("filler {i}"), ContextKind::Note),
                i + 2,
                1,
                None,
            ));
        }
        stored_id
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::CheckedFiles {
                files: vec!["AuthService.rs@abc".into()],
            },
        })
        .await
        .unwrap();

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .all(|item| !item.content.contains(BODY)),
        "the skipped Stored body must not re-enter SELECTED WORKING CONTEXT"
    );
    let descriptor = materialized
        .external
        .iter()
        .find(|entry| entry.item_id == stored_id)
        .expect(
            "a Checked Stored file body far past the recency tail must surface \
             through the entity index",
        );
    assert_eq!(descriptor.context_ref.summary, "AuthService.rs@abc");
    assert!(
        !descriptor.context_ref.summary.contains(BODY),
        "refs only: identity, not the file text"
    );
}

/// A mid-turn working-set signal extends the hot-entity set so the next
/// model round can recall evidence, without creating an item — the body is
/// persisted later, at turn end, by the runtime.
#[tokio::test]
async fn working_set_signal_extends_hot_entities_without_creating_a_body() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "start".into(),
        })
        .await
        .unwrap();
    let items_before = engine.state.lock().await.items.len();

    engine
        .ingest(ContextIngress::WorkingSetSignal {
            resources: vec![
                agent_contracts::ResourceTouch {
                    path: "src/AuthService.rs".into(),
                    revision: None,
                },
                agent_contracts::ResourceTouch {
                    path: "src/CacheStore.rs".into(),
                    revision: None,
                },
            ],
            content: "discovered AuthService.rs and CacheStore.rs".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    assert!(
        state.hot_entities.iter().any(|e| e == "src/AuthService.rs"),
        "the signal's resource paths must become hot for the next round: {:?}",
        state.hot_entities
    );
    assert!(
        state.hot_entities.iter().any(|e| e == "src/CacheStore.rs"),
        "every signaled resource must be merged: {:?}",
        state.hot_entities
    );
    assert_eq!(
        state.items.len(),
        items_before,
        "a working-set signal must not create an item"
    );
}

#[tokio::test]
async fn anchor_roots_directive_replaces_the_root_set_and_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let claim = |item_ref: &str| agent_contracts::AnchorRootClaim {
        item_ref: item_ref.to_string(),
        strength: agent_contracts::AnchorRootStrength::ResidentRequired,
        source_field_id: "working_refs".into(),
        ..Default::default()
    };

    // 推送一组根声明：整组替换（不是逐条增删），engine 只镜像投影。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![claim("context://run/abc"), claim("AuthService.rs")],
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.anchor_roots.len(), 2);
        assert_eq!(state.anchor_roots[0].item_ref, "context://run/abc");
        assert_eq!(state.anchor_roots[1].source_field_id, "working_refs");
    }

    // 再推送会替换整组，而不是合并。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![claim("CacheStore.rs")],
            },
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.anchor_roots.len(), 1, "整组替换，不是合并");
    }

    // 超出上限的推送被拒绝，且不改变现有根集。
    let too_many: Vec<_> = (0..=agent_contracts::MAX_ANCHOR_ROOT_CLAIMS)
        .map(|i| claim(&format!("item-{i}")))
        .collect();
    let error = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots { roots: too_many },
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("exceed the cap"),
        "超限必须拒绝：{error}"
    );
    let state = engine.state.lock().await;
    assert_eq!(state.anchor_roots.len(), 1, "被拒绝的推送必须保持原根集");
}

#[tokio::test]
async fn anchor_root_claims_protect_items_from_gc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "protected decision record".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.2,
            Some("tool-capture".into()),
        );
        item.id = ContextItemId::new();
        item.attention = AttentionState::Archived;
        // 世代已到上限：无声明时 GC 会把它 evict（基线对照）。
        item.gc_generation = 3;
        let id = item.id;
        state.items.push(item);
        id
    };
    // 基线：无声明时，这个已冷却的低分条目会被 GC evict。
    let baseline = engine.gc().await.unwrap();
    assert!(
        baseline.evicted >= 1,
        "无声明时该条目必须是 eviction 候选：{baseline:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|i| i.id == target_id),
        "无声明时条目已离开 heap"
    );
    drop(state);

    // 推送 ResidentRequired 声明，下一次 GC 把它召回并保护在 heap。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 4,
                    reason: agent_contracts::RootReason::OpenLoop,
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.anchor_roots_protected >= 1,
        "报告必须说明声明保护的条目数：{report:?}"
    );
    assert!(
        report.anchor_root_protections.iter().any(|protection| {
            protection.item_ref == target_id.to_string()
                && protection.source_field_id == "working_refs"
                && protection.anchor_revision == 4
                && protection.reason == agent_contracts::RootReason::OpenLoop
                && protection.strength == agent_contracts::AnchorRootStrength::ResidentRequired
        }),
        "每个 residency 根必须报告 revision + source_field + RootReason：{report:?}"
    );
    let state = engine.state.lock().await;
    let resident = state
        .items
        .iter()
        .find(|i| i.id == target_id)
        .expect("声明保护的条目必须回到 resident heap");
    assert_eq!(
        resident.attention,
        AttentionState::Active,
        "召回重置 attention"
    );
}

#[tokio::test]
async fn anchor_root_claims_reactivate_evicted_items_from_the_warm_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "evicted working finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.3,
            Some("tool-capture".into()),
        );
        item.id = ContextItemId::new();
        item.evicted_at_tick = Some(1);
        item.residency = agent_contracts::ContextResidency::Warm;
        let id = item.id;
        state.eviction_buffer.push(item);
        id
    };
    // 无声明时，buffer 里的低分条目留在 buffer。
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|i| i.id == target_id),
            "无声明时 buffer 条目不被召回"
        );
    }

    // 推送 PromptRequired 声明（同样要求 resident），下一次 GC 从 buffer
    // 召回并给出可解释的 reason。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::PromptRequired,
                    source_field_id: "open_loops".into(),
                    ..Default::default()
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivations.iter().any(|r| r.item_id == target_id),
        "声明指向的 buffer 条目必须被召回：{report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        state.items.iter().any(|i| i.id == target_id),
        "条目必须回到 resident heap"
    );
}

#[tokio::test]
async fn anchor_root_claims_never_resurrect_terminal_items() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "superseded decision".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("tool-capture".into()),
        );
        item.id = ContextItemId::new();
        item.semantic = SemanticState::Tombstoned;
        let id = item.id;
        state.eviction_buffer.push(item);
        id
    };
    // 即使声明要求 resident，terminal 语义也是终态：不复活。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                    ..Default::default()
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        !report.reactivations.iter().any(|r| r.item_id == target_id),
        "terminal 条目永不复活：{report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|i| i.id == target_id),
        "terminal 条目必须留在 buffer"
    );
}

#[tokio::test]
async fn prompt_required_anchor_roots_force_selection() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "required constraint note".into(),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.1,
            None,
        );
        item.id = ContextItemId::new();
        item.attention = AttentionState::Archived;
        let id = item.id;
        state.items.push(item);
        id
    };
    // 基线：低分 Archived 条目不在候选里（session scope 无 active
    // scope 匹配、非 hot），materialize 不选中它。
    let baseline = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        !baseline.items.iter().any(|m| m.item_id == target_id),
        "无声明时低分条目不被选中"
    );

    // PromptRequired 声明强制它进帧。
    let materialized = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: agent_contracts::ContextHints {
                max_selected_items: None,
                anchor_roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::PromptRequired,
                    source_field_id: "constraints".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let selected = materialized
        .items
        .iter()
        .find(|m| m.item_id == target_id)
        .expect("PromptRequired 声明必须强制条目进帧");
    assert_eq!(
        selected.retention,
        ContextRetention::Working,
        "进帧的是同一个条目"
    );
}

#[tokio::test]
async fn prompt_required_item_is_packed_once_when_also_scored() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // Session-scope Active 条目本来就是打分候选；再叠加 PromptRequired
    // 声明时，优先轮已选入，普通候选轮不得重复选入或重复扣预算。
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "active scored note the anchor also requires".into(),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.8,
            None,
        );
        item.id = ContextItemId::new();
        item.attention = AttentionState::Active;
        let id = item.id;
        state.items.push(item);
        id
    };
    let materialized = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: agent_contracts::ContextHints {
                max_selected_items: None,
                anchor_roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::PromptRequired,
                    source_field_id: "constraints".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let occurrences = materialized
        .items
        .iter()
        .filter(|m| m.item_id == target_id)
        .count();
    assert_eq!(
        occurrences, 1,
        "PromptRequired 非 Pinned 条目只能被装帧一次（优先轮选入后普通轮跳过）"
    );
}

#[tokio::test]
async fn storage_required_anchor_roots_protect_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        storage_ttl_ticks: 1,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // 构造一个已 dead 的外部条目（Working retention，TTL 过期即可删）。
    let seed_dead = |state: &mut crate::engine::State, tick: u64| {
        let mut item = crate::item::make_item(
            state,
            &engine.config,
            "retired evidence blob".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            None,
        );
        item.id = ContextItemId::new();
        item.semantic = SemanticState::VerifiedFixed { by: None };
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, tick, 1, None,
        ));
        item.id
    };

    // 场景 A：无声明时，dead 条目按 TTL 被 storage GC 删除。
    let unprotected_id = {
        let mut state = engine.state.lock().await;
        seed_dead(&mut state, 0)
    };
    let baseline = engine.storage_gc().await.unwrap();
    assert!(
        baseline.deleted >= 1,
        "无声明时 dead 条目必须可删除：{baseline:?}"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            !state.external.iter().any(|e| e.item_id == unprotected_id),
            "无声明的 dead 条目已被删除"
        );
    }

    // 场景 B：推送 StorageRequired 声明，storage GC 保留同一种条目。
    let protected_id = {
        let mut state = engine.state.lock().await;
        seed_dead(&mut state, 1)
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: protected_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::StorageRequired,
                    source_field_id: "evidence_refs".into(),
                    anchor_revision: 2,
                    reason: agent_contracts::RootReason::CompletionEvidence,
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.storage_gc().await.unwrap();
    assert!(
        report.anchor_roots_protected >= 1,
        "报告必须说明声明保护的 store 条目：{report:?}"
    );
    assert!(
        report.anchor_root_protections.iter().any(|protection| {
            protection.item_ref == protected_id.to_string()
                && protection.source_field_id == "evidence_refs"
                && protection.anchor_revision == 2
                && protection.reason == agent_contracts::RootReason::CompletionEvidence
                && protection.strength == agent_contracts::AnchorRootStrength::StorageRequired
        }),
        "每个 storage 根必须报告 revision + source_field + RootReason：{report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        state.external.iter().any(|e| e.item_id == protected_id),
        "StorageRequired 声明指向的条目必须保留"
    );
}

#[tokio::test]
async fn task_anchor_view_is_not_copied_onto_materialize() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let view = agent_contracts::TaskAnchorView {
        revision: 7,
        original_goal: "refactor auth".into(),
        current_interpretation: "split the module".into(),
        constraints: vec!["keep the public API".into()],
        ..Default::default()
    };
    let materialized = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: agent_contracts::ContextHints {
                task: Some(view.clone()),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(
        materialized.task.is_none() && materialized.focus.is_none(),
        "engine materialize is historical working context; TaskAnchor/Focus stay on the runtime assembler"
    );
    assert!(
        materialized
            .items
            .iter()
            .all(|item| item.content != "refactor auth"),
        "view 不是 heap 条目"
    );
}

#[tokio::test]
async fn resident_required_does_not_force_prompt_selection() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "resident-only note".into(),
            ContextKind::Note,
            ContextScope::Session,
            ContextRetention::Working,
            0.1,
            None,
        );
        item.id = ContextItemId::new();
        item.attention = AttentionState::Archived;
        let id = item.id;
        state.items.push(item);
        id
    };
    let materialized = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(
        !materialized
            .items
            .iter()
            .any(|item| item.item_id == target_id),
        "ResidentRequired 不是 prompt 根：{materialized:?}"
    );
}

#[tokio::test]
async fn storage_required_is_not_a_residency_root() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let target_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "storage-only evidence".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.2,
            Some("tool-capture".into()),
        );
        item.id = ContextItemId::new();
        item.attention = AttentionState::Archived;
        item.gc_generation = 3;
        let id = item.id;
        state.items.push(item);
        id
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: target_id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::StorageRequired,
                    source_field_id: "evidence_refs".into(),
                    ..Default::default()
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.anchor_roots_protected, 0,
        "StorageRequired 不得充当 residency 根：{report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == target_id),
        "StorageRequired 条目可以离开 heap"
    );
}

/// Admit of an externalized item re-reads the owner's blob; a tampered body
/// (valid JSON, same id, changed content) under the original checksum must
/// fail the admit instead of substituting changed content into the working
/// set.
#[tokio::test]
async fn admit_rejects_a_tampered_blob() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        recent_file_bodies: 1,
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
    let files = ["AuthService.rs", "CacheStore.rs", "TokenCache.rs"];
    for (i, path) in files.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: fs_read_touching(
                    &format!("step-{i}"),
                    path,
                    &format!("     1 | step {i}: fix {path}"),
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
    let gc_report = engine.gc().await.unwrap();
    assert_eq!(
        gc_report.externalized, 1,
        "overflow must externalize: {gc_report:?}"
    );
    let target = gc_report.externalized_ids[0];

    // Tamper the blob under the same id, then admit it.
    std::fs::write(
        dir.path().join(format!("{target}.json")),
        serde_json::to_vec(&agent_contracts::ContextItem {
            content: "substituted body".into(),
            ..engine
                .fetch_external(target)
                .await
                .expect("the pre-tamper fetch must resolve")
                .expect("the item must still be external")
        })
        .unwrap(),
    )
    .unwrap();
    let failure = engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: target,
                reason: "the model needs this step again".into(),
            },
        })
        .await
        .unwrap_err();
    assert!(
        failure.to_string().contains("corrupt"),
        "the tampered admit must fail as corruption, got: {failure}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == target),
        "the substituted body must never enter the working set"
    );
}

/// An export that cannot commit its artifact must not lose the taken rows:
/// they merge back (FIFO, bounded) and a later export persists them.
#[tokio::test]
async fn failed_ledger_export_merges_rows_back() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
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
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: observation_touching("step-0", true, "step: read view", Some("AuthService.rs")),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    // The commit target is a directory, so the temp -> target rename fails
    // and the export must report the failure instead of wiping its rows.
    let target = dir.path().join("lifecycle.jsonl");
    std::fs::create_dir_all(&target).unwrap();
    let error = engine
        .export_ledger(&target)
        .await
        .expect_err("a directory target must fail the export");
    assert!(
        error.to_string().contains("commit ledger artifact"),
        "{error}"
    );

    // The taken rows came back: a later export to a writable path persists
    // exactly what the failed one took.
    let retry = dir.path().join("retry.jsonl");
    let count = engine.export_ledger(&retry).await.unwrap();
    assert!(
        count >= 2,
        "the failed export must not lose rows, got {count}"
    );
    let text = std::fs::read_to_string(&retry).unwrap();
    assert_eq!(
        text.lines().count(),
        count,
        "every persisted row must be one artifact line"
    );
}

/// The admit store read runs outside the state lock: while a large blob is
/// read back, unrelated context work (diagnostics) completes without
/// waiting for the read. A lock-across-IO regression makes diagnostics
/// queue behind the whole read, so the timing split disappears.
#[tokio::test]
async fn admit_store_read_does_not_block_unrelated_context_work() {
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
    // One oversized observation feeds the eviction queue first, so the
    // buffer overflow externalizes it and reading it back measurably
    // outlives any lock section.
    let large = format!(
        "step 0: fix AuthService.rs {}",
        "y".repeat(32 * 1024 * 1024)
    );
    let observations = [
        observation_touching("step-0", true, &large, Some("AuthService.rs")),
        observation_touching("step-1", true, "step: read view", Some("CacheStore.rs")),
        observation_touching("step-2", true, "step: token cache", Some("TokenCache.rs")),
    ];
    for output in observations {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output,
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
        "overflow must externalize: {gc_report:?}"
    );
    let target = gc_report.externalized_ids[0];

    let t0 = std::time::Instant::now();
    let admit = engine.ingest(ContextIngress::ContextDirective {
        action: ContextAction::Admit {
            item_id: target,
            reason: "the model needs this step again".into(),
        },
    });
    let diag = engine.diagnostics();
    tokio::pin!(admit);
    tokio::pin!(diag);
    let mut diag_done = None;
    let mut admit_done = None;
    while diag_done.is_none() || admit_done.is_none() {
        tokio::select! {
            outcome = &mut diag, if diag_done.is_none() => {
                outcome.unwrap();
                diag_done = Some(t0.elapsed());
            }
            outcome = &mut admit, if admit_done.is_none() => {
                outcome.unwrap();
                admit_done = Some(t0.elapsed());
            }
        }
    }
    let diag_done = diag_done.unwrap();
    let admit_total = admit_done.unwrap();
    assert!(
        diag_done < std::time::Duration::from_millis(300) && admit_total > diag_done * 3,
        "diagnostics must not queue behind the admit store read \
         (diagnostics {diag_done:?}, admit {admit_total:?})"
    );
}

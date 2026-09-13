//! CTX-9 (R2-07): closed scopes and historical metadata have a bounded
//! lifecycle. Closed, fully unreferenced scope nodes retire (the tree, the
//! in-memory footprint and the checkpoint stop growing with finished tool
//! frames and tasks), while each retirement keeps a bounded fact note —
//! so a completed task's completion fact survives its scope node and the
//! task's records are never misjudged as unfinished. Referenced scopes and
//! their ancestor chains are never retired (no dangling parents).

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextAction, ContextEngine, ContextIngress, ContextKind,
    ContextMaintenanceTrigger, ContextResidency, ContextRetention, ContextScope, RootReason,
    ScopeKind,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::{open_focus, tool_observation};

#[tokio::test]
async fn ten_thousand_tool_scope_cycles_keep_the_tree_and_checkpoint_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 64,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "long tool sequence").await;

    for cycle in 0..10_000 {
        let scope = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
        engine.close_scope(scope).await.unwrap();
        // Retire periodically, exactly like the runtime's GC cadence.
        if cycle % 500 == 499 {
            engine.gc().await.unwrap();
        }
    }
    engine.gc().await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.scopes.len() <= 128,
        "the scope tree must stop growing with finished tool frames: {} scopes left",
        state.scopes.len()
    );
    assert!(
        !state.retired_scopes.is_empty(),
        "retirements are recorded as bounded fact notes"
    );
    assert!(
        state.retired_scopes.len() <= crate::scope::MAX_RETIRED_SCOPE_NOTES,
        "the retirement ring stays bounded"
    );
    // The checkpoint no longer serializes one node per finished frame.
    drop(state);
    let checkpoint = engine.checkpoint().await.unwrap();
    let serialized = serde_json::to_string(&checkpoint).unwrap();
    assert!(
        serialized.len() < 512 * 1024,
        "the checkpoint must stay bounded across 10k closed scopes: {} bytes",
        serialized.len()
    );
}

#[tokio::test]
async fn a_completed_task_fact_survives_its_scope_retirement() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Target 2 so the final session+task+focus tree (3 nodes) is above
        // the retirement gate and the dead chain actually retires.
        scope_retire_target: 2,
        gc_buffer_capacity: 0, // observations go straight to the store
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_id = open_focus(&engine, "finish the auth fix").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix the auth failure".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "t1", "auth failure reproduced in AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    // Complete the task: the task scope and its descendants close, the
    // stored entry's chain stamp is released (the promotion boundary
    // passed), and the next pass retires the whole unreferenced chain.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "final round".into(),
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let task_scope = state
            .scopes
            .iter()
            .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_id))
            .map(|scope| scope.id)
            .expect("the task scope exists");
        let focus_scopes = state
            .scopes
            .iter()
            .filter(|scope| scope.kind == ScopeKind::Focus && scope.task_id == Some(task_id))
            .map(|scope| scope.id)
            .collect::<Vec<_>>();
        drop(state);
        engine.close_scope(task_scope).await.unwrap();
        for scope in focus_scopes {
            engine.close_scope(scope).await.unwrap();
        }
    }
    // Pass 1 evicts the closed-scope members and lands their entries with
    // released stamps; pass 2 sees an unreferenced chain and retires it.
    engine.gc().await.unwrap();
    let report = engine.gc().await.unwrap();

    let state = engine.state.lock().await;
    assert!(
        !state
            .scopes
            .iter()
            .any(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_id)),
        "the completed task's chain must retire once nothing references it: {} scopes left",
        state.scopes.len()
    );
    assert!(
        state
            .retired_scopes
            .iter()
            .any(|note| note.kind == ScopeKind::Task && note.task_id == Some(task_id)),
        "the completion fact must be on record when the node retires: {report:?}"
    );
    assert!(
        crate::scope::task_completion_recorded(&state, task_id),
        "task_completed semantics hold after the node left the tree"
    );
    // The stored entry released its chain stamp (the entry pins nothing).
    let entry = state
        .external
        .iter()
        .find(|entry| {
            entry
                .context_ref
                .summary
                .contains("auth failure reproduced")
        })
        .expect("the observation is stored");
    assert_eq!(
        entry.scope_id, None,
        "a completed task's stored entry pins no scope chain"
    );
}

#[tokio::test]
async fn a_completed_tasks_records_are_not_recalled_after_retirement() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 4,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_id = open_focus(&engine, "complete and retire").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "t1", "body about AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        let task_scope = state
            .scopes
            .iter()
            .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_id))
            .map(|scope| scope.id)
            .unwrap();
        drop(state);
        engine.close_scope(task_scope).await.unwrap();
    }
    engine.gc().await.unwrap();

    // Heat the finished task's entity again with a brand-new task.
    let _new_task = open_focus(&engine, "a different task mentioning AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "new work touching AuthService.rs".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();

    assert!(
        report.reactivated == 0,
        "a retired completed task's records must not auto-recall: {report:?}"
    );
    let state = engine.state.lock().await;
    let recorded = crate::scope::task_completion_recorded(&state, task_id);
    assert!(
        recorded,
        "the completion fact must hold after retirement (live scope or note)"
    );
}

#[tokio::test]
async fn retirement_never_orphans_a_referenced_scope() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 4,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "hold the chain").await;
    // A live heap item pinned to the focus scope: the focus scope and its
    // whole ancestor chain must survive any retirement pass.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "the pinned body".into(),
        })
        .await
        .unwrap();
    let (focus_scope, task_scope) = {
        let state = engine.state.lock().await;
        let focus = state.active_scope_id.unwrap();
        let task = state
            .scopes
            .by_id(focus)
            .and_then(|scope| scope.parent)
            .unwrap();
        (focus, task)
    };
    // Flooding closed tool frames must not touch the referenced chain.
    for _ in 0..200 {
        let scope = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
        engine.close_scope(scope).await.unwrap();
    }
    engine.gc().await.unwrap();

    let state = engine.state.lock().await;
    assert!(
        state.scopes.by_id(focus_scope).is_some(),
        "a referenced focus scope is never retired"
    );
    assert!(
        state.scopes.by_id(task_scope).is_some(),
        "the ancestor of a referenced scope is never retired (no dangling parents)"
    );
    assert_eq!(
        state.scopes.by_id(focus_scope).unwrap().parent,
        Some(task_scope),
        "the chain stays intact"
    );
}

#[tokio::test]
async fn many_tasks_complete_retire_and_survive_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 8,
        gc_buffer_capacity: 0, // observations go straight to the store
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });

    // Twelve sequential tasks, each completed and retired: the tree and
    // the checkpoint must stay bounded while every completion fact stays
    // addressable across the retirement boundary.
    let mut task_ids = Vec::new();
    for round in 0..12u32 {
        let task_id = open_focus(&engine, &format!("task {round}")).await;
        task_ids.push(task_id);
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("task {round} works on Module{round}.rs"),
            })
            .await
            .unwrap();
        super::harness::tool_observation(
            &engine,
            &format!("call-{round}"),
            &format!("observation body {round}"),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        engine.gc().await.unwrap();

        // Complete the task: close its task scope and focus descendants.
        let (task_scope, focus_scopes) = {
            let state = engine.state.lock().await;
            let task_scope = state
                .scopes
                .iter()
                .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_id))
                .map(|scope| scope.id)
                .expect("the task scope exists");
            let focus_scopes = state
                .scopes
                .iter()
                .filter(|scope| scope.kind == ScopeKind::Focus && scope.task_id == Some(task_id))
                .map(|scope| scope.id)
                .collect::<Vec<_>>();
            (task_scope, focus_scopes)
        };
        engine.close_scope(task_scope).await.unwrap();
        for scope in focus_scopes {
            engine.close_scope(scope).await.unwrap();
        }
        // Pass 1 releases the entries' chain stamps; pass 2 retires.
        engine.gc().await.unwrap();
        engine.gc().await.unwrap();
    }

    {
        let state = engine.state.lock().await;
        assert!(
            state.scopes.len() <= 32,
            "the tree stays bounded across many completed tasks: {} scopes",
            state.scopes.len()
        );
        for task_id in &task_ids {
            assert!(
                crate::scope::task_completion_recorded(&state, *task_id),
                "every completion fact stays on record after its node retired"
            );
        }
    }

    // The checkpoint carries the retirement notes across the boundary.
    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint).await.unwrap();

    let state = engine.state.lock().await;
    for task_id in &task_ids {
        assert!(
            crate::scope::task_completion_recorded(&state, *task_id),
            "the restored engine still holds the completion fact (live scope or restored note)"
        );
    }
    assert!(
        state.scopes.len() <= 32,
        "the restored tree is bounded too: {} scopes",
        state.scopes.len()
    );
}

/// CTX-10 (R3-02): a scope the current owner *released* must not be brought
/// back by the stale blob. The real chain: externalize → close → retire →
/// claim-driven recall → checkpoint/restore. The old merge rule treated
/// "entry None" as "legacy missing" and kept the blob's stamp, so the
/// recalled body re-entered the heap pointing at a retired scope and the
/// engine's own checkpoint could not restore.
#[tokio::test]
async fn a_retired_scope_is_not_brought_back_by_its_blob() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Target 2: the post-close tree is session+task+focus (3 nodes) so
        // the retirement gate opens for the closed focus.
        scope_retire_target: 2,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "retire then recall").await;
    let (id, focus_scope) = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "claim-held body about Zephyr.rs".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.6,
            Some("test".into()),
        );
        item.scope_id = state.active_scope_id;
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        let focus_scope = state.active_scope_id.unwrap();
        let id = item.id;
        state.eviction_buffer.push(item);
        (id, focus_scope)
    };
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(state.external.get(id).is_some(), "setup: externalized");
    }
    // Close the focus scope; the close releases the entry's chain stamp and
    // the next passes retire the now-unreferenced scope node.
    engine.close_scope(focus_scope).await.unwrap();
    engine.gc().await.unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.scopes.iter().any(|scope| scope.id == focus_scope),
            "setup: the focus scope must be retired"
        );
        assert_eq!(
            state.external.get(id).unwrap().scope_id,
            None,
            "setup: the entry released its chain stamp"
        );
    }

    // Recall through a current residency-strength claim.
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::AnchorRoots {
                roots: vec![AnchorRootClaim {
                    item_ref: id.to_string(),
                    strength: AnchorRootStrength::ResidentRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 2,
                    reason: RootReason::CompletionEvidence,
                }],
            },
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    let _ = &report;

    {
        let state = engine.state.lock().await;
        let recalled = state
            .items
            .iter()
            .find(|item| item.id == id)
            .expect("the claim-held body is recalled: {report:?}");
        assert!(
            recalled
                .scope_id
                .is_none_or(|sid| state.scopes.by_id(sid).is_some()),
            "the recalled body must not reference a retired scope"
        );
    }
    // The engine's own checkpoint must stay restorable.
    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint).await.unwrap();
}

/// CTX-10 (R3-04): the retirement ring is chronological — a full ring must
/// drop its OLDEST facts and keep the newest — and once the ring has
/// overflowed, a task with no live scope and no note is treated
/// conservatively as completed: its retained bodies never regain automatic
/// hot-entity recall just because the bounded window forgot it.
#[tokio::test]
async fn retirement_ring_keeps_newest_facts_and_completion_stays_conservative() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Target 3: a tool cycle's tree (session + ancient chain + tool = 4)
        // exceeds the gate, so every closed tool retires and fills the ring.
        scope_retire_target: 3,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });

    // An ancient task whose completion fact will be trimmed away last.
    let ancient = open_focus(&engine, "ancient task").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "ancient work on Zephyr.rs".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    close_task(&engine, ancient).await;
    engine.gc().await.unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            crate::scope::task_completion_recorded(&state, ancient),
            "setup: the ancient completion fact is on record"
        );
    }

    // Overflow the ring with tool-scope retirements.
    for _ in 0..513 {
        let scope = engine.open_scope(ScopeKind::Tool, None).await.unwrap();
        engine.close_scope(scope).await.unwrap();
        engine.gc().await.unwrap();
    }
    {
        let state = engine.state.lock().await;
        assert!(
            state.retirement_ring_overflowed,
            "setup: the ring has overflowed at least once"
        );
    }

    // A task completed NOW must keep its fact (the newest entry survives a
    // full ring, not the oldest).
    let fresh = open_focus(&engine, "fresh task").await;
    close_task(&engine, fresh).await;
    engine.gc().await.unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            crate::scope::task_completion_recorded(&state, fresh),
            "the newest retirement fact must survive a full ring"
        );
    }

    // The ancient task's retained body must not regain automatic recall:
    // its fact left the ring and its scope is retired — unknown means
    // conservatively completed.
    let _fresh_focus = open_focus(&engine, "new work heating Zephyr.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "new task touching Zephyr.rs again".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivated == 0,
        "a forgotten-but-finished task's body must not auto-recall: {report:?}"
    );
}

/// Close a task's scope and its focus descendants (the runtime-side
/// completion shape, applied directly).
async fn close_task(engine: &SimpleContextEngine, task_id: agent_contracts::TaskId) {
    let (task_scope, focus_scopes) = {
        let state = engine.state.lock().await;
        let task_scope = state
            .scopes
            .iter()
            .find(|scope| scope.kind == ScopeKind::Task && scope.task_id == Some(task_id))
            .map(|scope| scope.id)
            .expect("the task scope exists");
        let focus_scopes = state
            .scopes
            .iter()
            .filter(|scope| scope.kind == ScopeKind::Focus && scope.task_id == Some(task_id))
            .map(|scope| scope.id)
            .collect::<Vec<_>>();
        (task_scope, focus_scopes)
    };
    engine.close_scope(task_scope).await.unwrap();
    for scope in focus_scopes {
        engine.close_scope(scope).await.unwrap();
    }
}

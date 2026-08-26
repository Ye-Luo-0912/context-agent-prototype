use agent_contracts::{
    AgentError, ContextAction, ContextEngine, ContextIngress, ContextItemId, ContextKind,
    ContextMaintenanceTrigger,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

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
            facts: None,
            output: observation_output("big-1", true, &big),
            scope_id: None,
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
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

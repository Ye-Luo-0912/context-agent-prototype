use agent_contracts::{
    AttentionState, ContextAction, ContextConsumptionAck, ContextEngine, ContextHints,
    ContextIngress, ContextItemId, ContextKind, ContextMaintenanceTrigger, ContextQuery,
    ContextResidency, ContextRetention, ContextScope, OperationId, SemanticState, ToolOutput,
    TurnId,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

#[tokio::test]
async fn diagnostics_report_resident_heap_bytes() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "count bytes").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "abcdefghij".into(),
        })
        .await
        .unwrap();
    let expected: usize = engine
        .state
        .lock()
        .await
        .items
        .iter()
        .map(|item| item.content.len())
        .sum();
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.resident_items,
        engine.state.lock().await.items.len()
    );
    assert_eq!(diagnostics.resident_bytes, expected);
    assert!(diagnostics.resident_bytes >= 10);
}

#[tokio::test]
async fn successful_observation_is_ephemeral_but_failure_persists_until_verified() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "auth task").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();

    // Round 1: failure — persists (Working) so a later fix can be verified.
    // The trusted recipe whose gate failed records the error with its own
    // identity (M17-B3/F08).
    verify_failure_observation(&engine, "1", "error in AuthService.rs:42", "auth.tests").await;
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

    // Round 2: a successful read of an error-related file is NOT proof the
    // error is fixed — entity overlap alone must not verify anything.
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "2".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
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
        !report
            .transitions
            .iter()
            .any(|t| t.reason.contains("verified fixed")),
        "a successful read must not verify an error, got: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );

    // The same host probe in the same task verifies the fault.
    verify_observation(&engine, "3", "tests passed in AuthService.rs").await;
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

/// M17-B3/F08 acceptance: a successful run of a DIFFERENT recipe, or of a
/// plain tool whose output mentions the same file, must never finalize an
/// error it did not probe — the error stays live.
#[tokio::test]
async fn unrelated_successes_never_finalize_an_error() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "auth task").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    verify_failure_observation(&engine, "1", "error in AuthService.rs:42", "auth.tests").await;

    // A DIFFERENT recipe succeeds and even mentions the same file.
    verify_observation_for_recipe(
        &engine,
        "2",
        "auth.tests v2 passed in AuthService.rs",
        "auth.tests.v2",
    )
    .await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let error = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("the error item");
        assert!(
            !error.semantic.is_dead(),
            "another recipe's success must not finalize the error, got {:?}",
            error.semantic
        );
    }

    // A successful plain tool result on the same file is also only
    // correlation.
    tool_observation(&engine, "3", "read AuthService.rs: no problems seen").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let error = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("the error item");
        assert!(
            !error.semantic.is_dead(),
            "a plain tool success must not finalize the error, got {:?}",
            error.semantic
        );
    }

    // The SAME recipe succeeding is the one proof that finalizes.
    verify_observation_for_recipe(&engine, "4", "tests passed in AuthService.rs", "auth.tests")
        .await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        let error = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error);
        assert!(
            error.is_none_or(|item| item.semantic.is_dead()),
            "the same recipe's success must finalize the error"
        );
    }
}

#[tokio::test]
async fn verification_requires_the_same_task_revision_and_definition_across_restore_and_tiers() {
    for tier in ["resident", "warm", "external"] {
        let dir = tempfile::tempdir().unwrap();
        let engine = SimpleContextEngine::new(SimpleContextConfig {
            context_store_dir: Some(dir.path().to_owned()),
            ..SimpleContextConfig::default()
        });
        let task = open_focus(&engine, "task A").await;
        verify_failure_observation(
            &engine,
            "failed",
            "failure in AuthService.rs:42",
            "auth.tests",
        )
        .await;
        let id = {
            let mut state = engine.state.lock().await;
            let item = state
                .items
                .iter()
                .find(|item| item.kind == ContextKind::Error)
                .unwrap()
                .clone();
            if tier != "resident" {
                let kept = state
                    .items
                    .iter()
                    .filter(|row| row.id != item.id)
                    .cloned()
                    .collect();
                state.items.replace_all(kept);
                if tier == "warm" {
                    state.eviction_buffer.push(item.clone());
                } else {
                    let reference = crate::store::externalize(dir.path(), &item).unwrap();
                    state.external.push(crate::store::to_external_entry(
                        &item, reference, 1, 1, None,
                    ));
                }
            }
            item.id
        };
        engine
            .restore(engine.checkpoint().await.unwrap())
            .await
            .unwrap();
        for (revision, digest, other_task) in [
            ("v2", "definition-v1", false),
            ("v1", "narrower-coverage", false),
            ("v1", "definition-v1", true),
        ] {
            if other_task {
                open_focus(&engine, "task B").await;
            }
            engine
                .ingest(ContextIngress::ToolObservation {
                    facts: Some(Box::new(verification_facts("auth.tests", revision, digest))),
                    output: ToolOutput {
                        call_id: "pass".into(),
                        tool_name: "verify.run".into(),
                        ok: true,
                        summary: "pass".into(),
                        model_content: "pass".into(),
                        artifact_ref: None,
                        metadata: serde_json::json!({"recipe_id": "auth.tests"}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            assert!(
                !engine
                    .state
                    .lock()
                    .await
                    .pending_verifications
                    .iter()
                    .any(|row| row.0 == id),
                "{tier}: unrelated PASS queued a terminal transition"
            );
        }
        engine
            .ingest(ContextIngress::FocusChanged {
                focus: agent_contracts::FocusState::for_task(task, "task A"),
            })
            .await
            .unwrap();
        verify_observation(&engine, "valid-pass", "fixed").await;
        assert!(
            engine
                .state
                .lock()
                .await
                .pending_verifications
                .iter()
                .any(|row| row.0 == id),
            "{tier}: matching PASS must reach the original fault"
        );
        engine
            .restore(engine.checkpoint().await.unwrap())
            .await
            .unwrap();
        assert!(
            engine
                .maintain(ContextMaintenanceTrigger::AfterTool)
                .await
                .unwrap()
                .transitions
                .iter()
                .any(|row| row.item_id == id && row.reason.contains("verified fixed")),
            "{tier}: valid queued evidence must survive restore"
        );
    }
}

#[tokio::test]
async fn legacy_pending_verification_cannot_bypass_probe_validation_after_restore() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "legacy task").await;
    verify_failure_observation(&engine, "failed", "failure", "auth.tests").await;
    tool_observation(&engine, "unrelated", "apparently passed").await;
    let id = {
        let mut state = engine.state.lock().await;
        let id = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .unwrap()
            .id;
        let by = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .unwrap()
            .id;
        state
            .pending_verifications
            .push((id, by, "legacy broad match".into()));
        id
    };
    engine
        .restore(engine.checkpoint().await.unwrap())
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterTool)
        .await
        .unwrap();
    assert!(
        !engine
            .state
            .lock()
            .await
            .items
            .iter()
            .find(|item| item.id == id)
            .unwrap()
            .semantic
            .is_dead()
    );
}

#[tokio::test]
async fn overlapping_error_text_is_not_a_recurrence_identity() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix auth").await;
    failed_observation(
        &engine,
        "one",
        "error in AuthService.rs:42: missing password",
    )
    .await;
    failed_observation(&engine, "two", "error in AuthService.rs:42: invalid token").await;
    let state = engine.state.lock().await;
    assert!(state.pending_supersessions.is_empty());
    assert_eq!(
        state
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::Error && !item.semantic.is_dead())
            .count(),
        2
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
            facts: None,
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
            foreground_item_ids: Vec::new(),
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
        foreground_item_ids: Vec::new(),
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
            foreground_item_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exactly one residency owner"));
    assert_eq!(engine.inspect(usize::MAX).await.unwrap()[0].access_count, 0);
}

#[tokio::test]
async fn foreground_consumption_is_recorded_without_reinforcing_access() {
    // the prompt rendered a foreground body, so the ack carries
    // its id. The engine records the consumption observably (diagnostics)
    // but must not reinforce access, Admit, or change residency —
    // foreground rehydration is transient by contract.
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::Pin {
            content: "pinned constraint".into(),
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
    assert!(!preview.items.is_empty());
    // Simulate one foreground body the materializer projected: the same
    // item, rehydrated transiently. Record it on the pending preview the
    // way `materialize` would have (the projection itself is driven by
    // runtime hints, not exercised here).
    let foreground_id = preview.items[0].item_id;
    {
        let mut state = engine.state.lock().await;
        let pending = state
            .pending_materialization
            .as_mut()
            .expect("materialize leaves a pending preview");
        pending.foreground_item_ids.insert(foreground_id);
    }
    let access_before = engine.inspect(usize::MAX).await.unwrap()[0].access_count;

    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: preview.materialization_id,
            item_ids: Vec::new(),
            external_item_ids: Vec::new(),
            foreground_item_ids: vec![foreground_id],
        })
        .await
        .unwrap();

    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(
        diagnostics.foreground_consumed_acks, 1,
        "foreground consumption must be observable"
    );
    assert_eq!(
        engine.inspect(usize::MAX).await.unwrap()[0].access_count,
        access_before,
        "foreground consumption is a weak signal: no access reinforcement"
    );

    // A foreground id outside the referenced preview fails closed.
    let preview = engine
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 8_192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    let error = engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: preview.materialization_id,
            item_ids: Vec::new(),
            external_item_ids: Vec::new(),
            foreground_item_ids: vec![ContextItemId::new()],
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("foreground"));
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

/// F18: `inspect(0)` takes the early-exit path — an empty catalog with no
/// projection work at all, whatever the heap, the warm buffer and the
/// store hold.
#[tokio::test]
async fn inspect_zero_limit_projects_nothing() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    for i in 0..5 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("message {i}"),
            })
            .await
            .unwrap();
    }
    assert!(engine.inspect(0).await.unwrap().is_empty());
}

/// F18: the bounded catalog is a lazy projection over heap, warm buffer
/// and store, with the pre-refactor order preserved: `inspect(limit)`
/// returns exactly the `limit` smallest created_ticks across *all* body
/// locations, ascending — the result equals a stable sort of the full
/// catalog truncated to `limit`.
#[tokio::test]
async fn inspect_small_limit_matches_the_sorted_full_catalog_across_locations() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // The tiny buffer overflows on the first GC pass, so the catalog
        // spans Resident, Warm and Stored rows.
        gc_buffer_capacity: 2,
        gc_max_generation: 0,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "catalog work").await;
    for i in 0..6 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("catalog row {i} Ticket{i}.rs"),
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    // Leave the episode: the old rows cool out of the working set and the
    // first GC pass evicts them, overflowing the tiny buffer to the store.
    open_focus(&engine, "other work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "something else entirely".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.eviction_buffer.is_empty() || !state.external.is_empty(),
            "the catalog must span more than the heap for this test to bite"
        );
    }

    let full = engine.inspect(usize::MAX).await.unwrap();
    assert!(full.len() >= 4, "a real catalog: {}", full.len());
    let mut expected: Vec<u64> = full.iter().map(|s| s.created_tick).collect();
    expected.sort();

    let limited = engine.inspect(4).await.unwrap();
    let got: Vec<u64> = limited.iter().map(|s| s.created_tick).collect();
    assert_eq!(
        got,
        expected[..4],
        "the limit picks the oldest rows ascending"
    );
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

/// F15: two compatible decisions about the same file share the semantic key
/// but withdraw nothing — "use X with a timeout" and "use X with logging"
/// must coexist. Entity overlap alone is a relevance signal, never proof,
/// and plain "use" carries no replacement cue.
#[tokio::test]
async fn compatible_decisions_about_one_file_coexist() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with structured logging".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("superseded by decision")),
        "compatible decisions must not supersede each other: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
    let state = engine.state.lock().await;
    let decisions: Vec<_> = state
        .items
        .iter()
        .filter(|item| item.content.contains("AuthService.rs"))
        .collect();
    assert_eq!(decisions.len(), 2, "both decisions exist");
    assert!(
        decisions.iter().all(|item| item.semantic.is_live()),
        "neither decision may be finalized: {:?}",
        decisions
            .iter()
            .map(|item| item.semantic)
            .collect::<Vec<_>>()
    );
}

/// F15: a decision from another task never finalizes this task's decision,
/// even when the message names the same file and carries an explicit
/// replacement cue. Only the same task context can prove a replacement.
#[tokio::test]
async fn cross_task_entity_overlap_does_not_supersede_decisions() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_a = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    let _task_b = open_focus(&engine, "billing work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "drop AuthService.rs from the plan".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let login_decision = state
        .items
        .iter()
        .find(|item| item.task_id == Some(task_a) && item.content.contains("use AuthService.rs"))
        .expect("task A's decision exists");
    assert!(
        login_decision.semantic.is_live(),
        "another task's decision must not finalize task A's decision, got {:?}",
        login_decision.semantic
    );
}

/// F15: an explicit replacement ("switch to Y instead of X") of the same
/// task supersedes the older line, and the Superseded state names the
/// replacing decision (`by`).
#[tokio::test]
async fn explicit_replacement_supersedes_and_names_the_replacement() {
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
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let old = state
        .items
        .iter()
        .find(|item| item.content == "use TOML for config")
        .expect("the replaced decision stays addressable");
    let new = state
        .items
        .iter()
        .find(|item| item.content == "switch to YAML instead of TOML")
        .expect("the replacing decision exists");
    assert_eq!(
        old.semantic,
        agent_contracts::SemanticState::Superseded { by: Some(new.id) },
        "the old decision must be superseded by the new one, got {:?}",
        old.semantic
    );
    assert!(new.semantic.is_live(), "the replacement stays live");
}

/// F02: a replacement cue plus a shared file must NOT finalize a different
/// requirement on that same file. The earlier line ("use AuthService.rs
/// with a 5-second timeout") and the later one ("replace plain-text logging
/// in AuthService.rs with structured logging") are two independent
/// constraints that happen to name one file. The later message contains the
/// cue "replace" and shares the entity `AuthService.rs`, but it withdraws
/// nothing about the timeout — changing a log format is not proof of
/// retracting a latency requirement.
///
/// F15 already stopped *cross-task* overlap and required an exact entity.
/// This is the remaining same-task hole: entity overlap is a relevance
/// signal for retrieval, never a permanent semantic revocation.
#[tokio::test]
async fn a_replace_cue_on_a_shared_file_does_not_withdraw_an_unrelated_requirement() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "replace plain-text logging in AuthService.rs with structured logging".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let timeout = state
        .items
        .iter()
        .find(|item| item.content.contains("5-second timeout"))
        .expect("the timeout requirement stays addressable");
    assert!(
        timeout.semantic.is_live(),
        "replacing the logging format must not retract the timeout requirement, got {:?}",
        timeout.semantic
    );
}

/// F02 control: a genuine withdrawal that names the same dimension still
/// finalizes the older decision, so the fix narrows only the unproven
/// overlap case and does not disable replacement entirely.
#[tokio::test]
async fn an_explicit_withdrawal_of_the_same_requirement_still_supersedes() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "replace the AuthService.rs 5-second timeout with a 30-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let old = state
        .items
        .iter()
        .find(|item| item.content.contains("5-second timeout") && item.content.contains("use "))
        .expect("the replaced decision stays addressable");
    assert!(
        !old.semantic.is_live(),
        "an explicit replacement of the same requirement must still withdraw it, got {:?}",
        old.semantic
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
                facts: None,
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

/// 来源权威跨外部化保留：外部化时 `source` 随条目进入 external map，
/// inspect 的 catalog 投影显示原始来源（而不是固定的 "externalized"
/// 占位），admit 把条目带回工作集后来源依然保持。这是 fetch/admit 时
/// 权威校验的前提——来源信息若在外部化时丢失，就无从校验。
#[tokio::test]
async fn externalized_source_survives_inspect_and_admit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = crate::engine::State::default();
        let config = SimpleContextConfig::default();
        let mut item = crate::item::make_item(
            &state,
            &config,
            "tool-captured finding: the cache layer is the hot path".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.6,
            Some("tool-capture".to_string()),
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        let value = crate::checkpoint::serialize(&state).unwrap();
        engine.restore(value).await.unwrap();
        item.id
    };

    // inspect 的 catalog 投影必须显示原始来源，而不是 "externalized" 占位。
    let catalog = engine.inspect(usize::MAX).await.unwrap();
    let entry = catalog
        .iter()
        .find(|item| item.id == item_id)
        .expect("the externalized entry is part of the logical catalog");
    assert_eq!(
        entry.source.as_deref(),
        Some("tool-capture"),
        "the source authority must survive externalization"
    );

    // admit 把条目带回工作集：来源保持，外部 map 移除。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id,
                reason: "the finding is relevant again".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let resident = state
        .items
        .iter()
        .find(|i| i.id == item_id)
        .expect("the item is resident after admit");
    assert_eq!(
        resident.source.as_deref(),
        Some("tool-capture"),
        "the source authority must survive the external -> resident move"
    );
    assert!(
        state.external.get(item_id).is_none(),
        "the entry must leave the external map"
    );
}

/// 权威元数据（打分权重/时钟/访问计数/GC 世代）跨外部化同构：外部化只搬运
/// body 到 store，权威元数据随条目保留——inspect 的 external 投影如实显示
/// 真实 importance/created_tick（而不是硬编码 0.0 或用 externalized_at_tick
/// 近似），admit 带回工作集后字段保持。这是 ContextCatalog 统一权威的前提。
#[tokio::test]
async fn externalized_authority_metadata_survives_externalization() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let item_id = {
        let mut state = crate::engine::State::default();
        let config = SimpleContextConfig::default();
        let mut item = crate::item::make_item(
            &state,
            &config,
            "metadata-preserving tool finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.7,
            Some("tool-capture".to_string()),
        );
        // 覆盖完整权威元数据：外部化后这些值必须原样可见。
        item.id = ContextItemId::new();
        item.relevance = 0.3;
        item.created_tick = 42;
        item.created_turn = 3;
        item.last_access_turn = 5;
        item.last_selected_turn = 4;
        item.access_count = 7;
        item.gc_generation = 2;
        item.evicted_at_tick = Some(10);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 99, 1, None,
        ));
        let value = crate::checkpoint::serialize(&state).unwrap();
        engine.restore(value).await.unwrap();
        item.id
    };

    // inspect 的 external 投影必须如实反映权威元数据（非 0.0、非
    // externalized_at_tick 近似、非 turn 0）。
    let catalog = engine.inspect(usize::MAX).await.unwrap();
    let entry = catalog
        .iter()
        .find(|item| item.id == item_id)
        .expect("the externalized entry is part of the logical catalog");
    assert_eq!(
        entry.importance, 0.7,
        "importance must survive externalization"
    );
    assert_eq!(
        entry.relevance, 0.3,
        "relevance must survive externalization"
    );
    assert_eq!(
        entry.created_tick, 42,
        "the real creation tick must be kept"
    );
    assert_eq!(entry.created_turn, 3, "the creation turn must be kept");
    assert_eq!(entry.last_access_turn, 5, "the access turn must be kept");
    assert_eq!(
        entry.last_selected_turn, 4,
        "the selection turn must be kept"
    );
    assert_eq!(entry.access_count, 7, "the access count must be kept");

    // admit 带回工作集：权威元数据经 blob 读回后原样保持。
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id,
                reason: "the finding is relevant again".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let resident = state
        .items
        .iter()
        .find(|i| i.id == item_id)
        .expect("the item is resident after admit");
    // 权威元数据在 body 移动（external -> resident）后原样保持：创建时钟、
    // 打分权重、入选时钟都是历史事实，admit 只改位置与访问时钟。
    assert_eq!(
        resident.importance, 0.7,
        "importance is authority and must survive"
    );
    assert_eq!(
        resident.created_tick, 42,
        "the creation tick is authority and must not be rewritten"
    );
    assert_eq!(
        resident.created_turn, 3,
        "the creation turn is authority and must survive"
    );
    assert_eq!(
        resident.last_selected_turn, 4,
        "the selection turn is authority and must survive"
    );
    // admit 的入场语义更新（与 GC reactivate 一致的既有行为）：相关性抬升、
    // 访问时钟刷新、计数递增、世代从新窗口开始、清除 eviction 标记。
    assert_eq!(
        resident.relevance, 0.5,
        "re-entry floors the relevance at 0.5"
    );
    assert_eq!(
        resident.last_access_turn, state.turn,
        "re-entry refreshes the access clock"
    );
    assert_eq!(resident.access_count, 8, "re-entry counts one more access");
    assert_eq!(
        resident.gc_generation, 0,
        "re-entry restarts the GC generation window"
    );
    assert_eq!(
        resident.evicted_at_tick, None,
        "the eviction marker is cleared on re-entry"
    );
}

/// PLATFORM-2：inspect 读模型区分「驻留 / 本轮实际发送 / 仅摘要指针」——
/// 驻留条目 residency=Resident；materialize 选中后（同回合）selected_current_turn
/// 为真；外部条目 residency=External（读者无 fetch 只见摘要指针）。
#[tokio::test]
async fn inspect_reports_residency_and_actual_send_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let external_id = {
        let mut state = crate::engine::State::default();
        let config = SimpleContextConfig::default();
        let mut item = crate::item::make_item(
            &state,
            &config,
            "externalized pointer-only finding".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.6,
            None,
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        let value = crate::checkpoint::serialize(&state).unwrap();
        engine.restore(value).await.unwrap();
        item.id
    };

    open_focus(&engine, "freshness").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "summarize the fresh facts".into(),
        })
        .await
        .unwrap();
    // 一个成功的观察条目：本轮的候选正文。
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: ToolOutput {
                call_id: "1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "the fresh fact: cache layer is the hot path".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            scope_id: None,
        })
        .await
        .unwrap();

    // 首次 inspect（本回合尚无 materialize）：驻留条目未发送，外部条目
    // 是指针。
    let catalog = engine.inspect(usize::MAX).await.unwrap();
    let external = catalog
        .iter()
        .find(|summary| summary.id == external_id)
        .expect("the external entry stays in the logical catalog");
    assert_eq!(external.residency, ContextResidency::External);
    assert!(!external.selected_current_turn);

    // materialize 选中驻留正文 → inspect 如实标记「本轮实际发送」。
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "summarize the fresh facts".into(),
            budget_tokens: 4096,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        !snapshot.items.is_empty(),
        "the working set must produce a surface"
    );
    let catalog = engine.inspect(usize::MAX).await.unwrap();
    for selected in &snapshot.items {
        let summary = catalog
            .iter()
            .find(|summary| summary.id == selected.item_id)
            .expect("every sent item stays visible in the catalog");
        assert_eq!(
            summary.residency,
            ContextResidency::Resident,
            "a sent body is a resident body"
        );
        assert!(
            summary.selected_current_turn,
            "an item in the latest surface must be marked actually-sent: {:?}",
            summary.last_selected_turn
        );
    }
}

/// CTX-2/E02 反例：`with Y instead of Z` 是范围化替换的介词结构——它点名的
/// 替代对象是 Z（logging），不是旧决策的要求（timeout）。共享文件＋出现
/// instead 不再构成整实体撤销，旧超时决策保持 live。
#[tokio::test]
async fn an_instead_of_phrase_names_its_own_object_not_the_whole_file() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with structured logging instead of plain-text logging"
                .into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("superseded by decision")),
        "a scoped instead-of phrase must not withdraw the unrelated timeout: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
    let state = engine.state.lock().await;
    let timeout = state
        .items
        .iter()
        .find(|item| item.content.contains("5-second timeout"))
        .expect("the timeout decision exists");
    assert!(
        timeout.semantic.is_live(),
        "the timeout requirement must stay live: {:?}",
        timeout.semantic
    );
}

/// CTX-2：一句包含多条要求——重申旧要求（追加新要求）不是替代宣告；
/// 替代对象点名的是 logging 维度，被重申的 timeout 不被终结。
#[tokio::test]
async fn one_message_with_several_requirements_restates_without_revoking() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content:
                "use AuthService.rs with a 5-second timeout and switch the logging to structured"
                    .into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("superseded by decision")),
        "restating a requirement is not a replacement declaration: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
}

/// CTX-2：否定/保留话术（"do not replace"、"keep"）不构成替代宣告——
/// 歧义先并存。
#[tokio::test]
async fn negated_or_retaining_wording_never_supersedes() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "do not replace the logging approach in AuthService.rs, and keep the timeout"
                .into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("superseded by decision")),
        "negated replacement wording must not supersede: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
}

/// CTX-2：中文追加条件不触发替代——并存（中文撤销支持不在本片范围，
/// 如实记录为限制）。
#[tokio::test]
async fn a_chinese_appended_condition_coexists() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "另外 AuthService.rs 的日志要改成结构化日志".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("superseded by decision")),
        "an appended Chinese condition must not supersede: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
}

/// CTX-2：Stored（外存摘要）与 Resident 同一判据——E02 反例消息对已
/// 外存的同文件超时决策同样不终结。
#[tokio::test]
async fn a_stored_decision_gets_the_same_scoped_instead_of_rule() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    // Put the decision into the external store directly (the same path a
    // real externalization uses), so the stored-summary rule can be probed.
    let stored_id = {
        let mut state = crate::engine::State::default();
        let config = SimpleContextConfig::default();
        let mut item = crate::item::make_item(
            &state,
            &config,
            "use AuthService.rs with a 5-second timeout".into(),
            ContextKind::Decision,
            ContextScope::Task,
            ContextRetention::Working,
            0.7,
            None,
        );
        item.id = ContextItemId::new();
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        let entry = crate::store::to_external_entry(&item, reference, 1, 1, None);
        state.external.push(entry);
        let value = crate::checkpoint::serialize(&state).unwrap();
        engine.restore(value).await.unwrap();
        item.id
    };
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with structured logging instead of plain-text logging"
                .into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();
    assert!(
        report
            .transitions
            .iter()
            .all(|t| !t.reason.contains("stored decision")),
        "a stored decision must survive the scoped instead-of phrase: {:?}",
        report
            .transitions
            .iter()
            .map(|t| &t.reason)
            .collect::<Vec<_>>()
    );
    let state = engine.state.lock().await;
    let entry = state
        .external
        .iter()
        .find(|entry| entry.item_id == stored_id)
        .expect("the stored decision stays in the store");
    assert!(
        entry.semantic.is_live(),
        "the stored timeout decision must stay live: {:?}",
        entry.semantic
    );
}

/// CTX-2 残余（R3-03）：修改超时*日志*不撤销超时*时长*。替换宾语与旧决策
/// 只共享一个内容词（timeout）不构成撤销证明——四个正文位置同判定，
/// 兼容要求保持 Live 共存。
#[tokio::test]
async fn modifying_timeout_logging_does_not_withdraw_the_timeout_requirement() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        gc_buffer_capacity: 0, // evictions go straight to the store
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth service requirements").await;
    let message = "use AuthService.rs with a 5-second timeout";

    // The same decision in all four body locations.
    engine
        .ingest(ContextIngress::UserMessage {
            content: message.into(),
        })
        .await
        .unwrap(); // Resident
    {
        let mut state = engine.state.lock().await;
        let make_copy = |state: &crate::engine::State| {
            let mut item = crate::item::make_item(
                state,
                &engine.config,
                message.into(),
                ContextKind::UserMessage,
                ContextScope::Task,
                ContextRetention::Working,
                0.62,
                Some("user".into()),
            );
            item.residency = ContextResidency::Warm;
            item.evicted_at_tick = Some(0);
            item
        };
        // Warm copy.
        let item = make_copy(&state);
        state.eviction_buffer.push(item);
        // Pending copy.
        let item = make_copy(&state);
        state.pending_externalize_retry.push(item);
        // Stored: the second copy externalizes on the next pass.
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            message.into(),
            ContextKind::UserMessage,
            ContextScope::Task,
            ContextRetention::Working,
            0.62,
            Some("user".into()),
        );
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        state.eviction_buffer.push(item);
    }
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.external.is_empty(),
            "setup: the stored copy must be externalized"
        );
    }

    // The scoped log-format change shares the dimension word "timeout" and
    // the file, but names neither the whole requirement nor all its words.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "replace timeout logging in AuthService.rs with structured events".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let mut live_copies = 0usize;
    for item in state
        .items
        .iter()
        .chain(state.eviction_buffer.iter())
        .chain(state.pending_externalize_retry.iter())
    {
        if item.content == message {
            assert!(
                item.semantic.is_live(),
                "the timeout requirement must stay live in every in-memory location: {:?}",
                item.semantic
            );
            live_copies += 1;
        }
    }
    for entry in state.external.iter() {
        if entry.context_ref.summary.contains("5-second timeout") {
            assert!(
                entry.semantic.is_live(),
                "the stored copy must not be finalized by a scoped log change"
            );
            live_copies += 1;
        }
    }
    assert!(
        live_copies >= 4,
        "all four copies must still exist: {live_copies} ({report:?})"
    );
}

/// 正面对照：替换宾语点名了旧要求的*全部*内容词（5-second timeout），
/// 明确撤销照常生效。
#[tokio::test]
async fn naming_the_full_requirement_still_supersedes() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs with a 5-second timeout".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "replace the 5-second timeout in AuthService.rs with a 30-second timeout"
                .into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::UserInput)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let old = state
        .items
        .iter()
        .find(|item| item.content == "use AuthService.rs with a 5-second timeout")
        .expect("the old decision exists");
    assert!(
        matches!(old.semantic, SemanticState::Superseded { .. }),
        "the explicit full-object withdrawal must supersede: {:?}",
        old.semantic
    );
}

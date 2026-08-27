use agent_contracts::{
    AttentionState, ContextAction, ContextConsumptionAck, ContextEngine, ContextHints,
    ContextIngress, ContextItemId, ContextKind, ContextMaintenanceTrigger, ContextQuery,
    ContextRetention, ContextScope, OperationId, ToolOutput, TurnId,
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
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();

    // Round 1: failure — persists (Working) so a later fix can be verified.
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
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
            facts: None,
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

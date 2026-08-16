use agent_contracts::{
    ContextConsumptionAck, ContextEngine, ContextIngress, ContextItemId, ContextKind,
    ContextMaintenanceTrigger, FocusState, MaterializedContext, OperationId, TaskId, ToolOutput,
    TurnId,
};

use crate::engine::SimpleContextEngine;

pub(crate) async fn acknowledge_all(
    engine: &SimpleContextEngine,
    materialized: &MaterializedContext,
) {
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
pub(crate) async fn open_focus(engine: &SimpleContextEngine, goal: &str) -> TaskId {
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, goal),
        })
        .await
        .unwrap();
    task_id
}

// ---------------------------------------------------------------------------
// Model/operator context directives: `ContextIngress::ContextDirective`
// applies gc hints, tags and leases; GC treats the targeted items as roots
// until the hint is cleared or the lease expires. Every protection is
// explainable in the eviction/reactivation reasons.
// ---------------------------------------------------------------------------

pub(crate) fn observation_output(id: &str, ok: bool, content: &str) -> ToolOutput {
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
pub(crate) async fn consumed_observation_outside_focus(
    engine: &SimpleContextEngine,
) -> ContextItemId {
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

/// Open a focus and produce `n` tool observations inside its task scope.
pub(crate) async fn observations_in_focus(
    engine: &SimpleContextEngine,
    n: usize,
) -> Vec<ContextItemId> {
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

/// Build a successful tool observation for a turn.
pub(crate) async fn tool_observation(engine: &SimpleContextEngine, call_id: &str, content: &str) {
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
pub(crate) async fn failed_observation(engine: &SimpleContextEngine, call_id: &str, content: &str) {
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

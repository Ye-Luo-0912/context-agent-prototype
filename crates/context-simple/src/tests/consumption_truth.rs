//! Consumption truth: the ack and miss ledger derive from the exact final
//! rendered frame. Runtime-trimmed bodies stop counting as previously
//! selected, foreground-only bodies the model consumed are attributed as
//! consumed on their next reread, and a clipped foreground body is an
//! explicit partial projection that cannot stand for the full revision.

use agent_contracts::{
    ContextConsumptionAck, ContextEngine, ContextHints, ContextIngress, ContextQuery, OperationId,
    ResourceKey, TurnId,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

#[tokio::test]
async fn ack_trimmed_item_stops_being_previously_selected() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix files").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect src/a.rs".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read_touching("1", "src/a.rs", "     1 | fn body() {}"),
            scope_id: None,
        })
        .await
        .unwrap();
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
            .any(|sel| sel.source.as_deref() == Some("tool:fs.read")),
        "the read must be selected before the trim"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.selected_body_paths.contains("src/a.rs"),
            "the preview exposure includes the selected body"
        );
    }

    // The runtime trimmed the body out of the final request: the ack
    // confirms it was never rendered.
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: snapshot.materialization_id,
            item_ids: vec![],
            external_item_ids: vec![],
            foreground_item_ids: vec![],
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read_touching("2", "src/a.rs", "     1 | fn body() {}"),
            scope_id: None,
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert!(
        !state.selected_body_paths.contains("src/a.rs"),
        "the trimmed body no longer counts as selected"
    );
    assert_eq!(
        state.reread_previously_selected, 0,
        "a body that never reached the model is not previously-selected"
    );
    assert_eq!(
        state.reread_resident_unselected, 1,
        "it reads back as a resident body the model has not consumed"
    );
}

#[tokio::test]
async fn acked_foreground_body_reads_back_as_previously_selected() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "append notes".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read_touching("bg-body", "src/bg.md", "     1 | fn body() {}"),
            scope_id: None,
        })
        .await
        .unwrap();
    // Demote the body to warm so it is not picked up by scored selection:
    // only the foreground projection exposes it.
    {
        let mut state = engine.state.lock().await;
        let mut items = state.items.take_all();
        let Some(pos) = items
            .iter()
            .position(|item| item.file_path.as_deref() == Some("src/bg.md"))
        else {
            panic!("the read must be resident");
        };
        let item = items.remove(pos);
        state.items.replace_all(items);
        state.eviction_buffer.push(item);
    }
    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "append notes".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                foreground_resources: vec![ResourceKey {
                    path: "src/bg.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(snapshot.foreground.len(), 1, "the warm body is projected");
    let foreground_id = snapshot.foreground[0].item_id;
    {
        let state = engine.state.lock().await;
        assert!(
            !state.selected_body_paths.contains("src/bg.md"),
            "a foreground-only body is not in the items-layer exposure"
        );
    }

    // The model consumed the foreground body; the ack carries only that id.
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: snapshot.materialization_id,
            item_ids: vec![],
            external_item_ids: vec![],
            foreground_item_ids: vec![foreground_id],
        })
        .await
        .unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.selected_body_paths.contains("src/bg.md"),
            "the acked foreground body enters the final-frame exposure"
        );
    }
    engine
        .ingest(ContextIngress::ToolObservation {
            facts: None,
            output: fs_read_touching("bg-reread", "src/bg.md", "     1 | fn body() {}"),
            scope_id: None,
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert_eq!(
        state.reread_previously_selected, 1,
        "a body the model consumed reads back as previously-selected"
    );
}

#[tokio::test]
async fn clipped_foreground_is_an_explicit_partial_body() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "append notes").await;
    {
        let mut state = engine.state.lock().await;
        let mut body = crate::item::make_item(
            &state,
            &engine.config,
            "x".repeat(20_000),
            agent_contracts::ContextKind::ToolObservation,
            agent_contracts::ContextScope::Session,
            agent_contracts::ContextRetention::Working,
            0.5,
            Some("tool:fs.read".into()),
        );
        body.scope_id = None;
        body.file_path = Some("src/scratch.md".into());
        state.items.push(body);
    }
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "Append to src/scratch.md".into(),
            budget_tokens: 10_000,
            hints: ContextHints {
                foreground_resources: vec![ResourceKey {
                    path: "src/scratch.md".into(),
                    revision: None,
                }],
                ..ContextHints::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(materialized.foreground.len(), 1);
    let body = &materialized.foreground[0];
    assert!(
        body.partial_body,
        "a clipped foreground body must be marked partial"
    );
    assert!(
        body.content.len() < 20_000,
        "the projected body is a prefix, not the full revision"
    );
    assert_eq!(
        body.file_path.as_deref(),
        Some("src/scratch.md"),
        "partial bodies keep their identity for display"
    );
}

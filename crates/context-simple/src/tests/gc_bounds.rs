//! Resource bounds at allocation time: report rows are capped with omitted
//! counters, failed overflow writes spill into a retry list instead of
//! growing the warm buffer past its cap, and the external view stops
//! collecting once the 32-row limit is reached.

use agent_contracts::{ContextEngine, ContextIngress, ContextQuery};

use crate::engine::{SimpleContextConfig, SimpleContextEngine, truncate_report_rows};

use super::harness::*;

#[test]
fn report_rows_are_capped_with_an_omitted_count() {
    let mut rows = Vec::new();
    for i in 0..10 {
        rows.push(format!("row {i}"));
    }
    let omitted = truncate_report_rows(&mut rows, 4);
    assert_eq!(omitted, 6);
    assert_eq!(rows.len(), 4, "the explainable rows stay within the budget");
    let none_omitted = truncate_report_rows(&mut rows, 4);
    assert_eq!(none_omitted, 0);
}

/// A store outage must not grow the warm buffer past its cap: failed
/// overflow writes spill into the retry list (content preserved) and the
/// next pass retries them without re-bloating the buffer.
#[tokio::test]
async fn failed_externalization_spills_into_retry_not_into_the_buffer() {
    // A regular *file* where the store directory should be: every blob write
    // fails with a filesystem error, the persistent-store-failure case.
    let dir = tempfile::tempdir().unwrap();
    let store_broken = dir.path().join("store");
    std::fs::write(&store_broken, b"not a directory").unwrap();

    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 2,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(store_broken),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "flood").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fill the working set".into(),
        })
        .await
        .unwrap();
    for i in 0..5 {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: observation_touching(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i}: nothing to see"),
                    None,
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let first = engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.len() <= 2,
            "the warm buffer must stay within its cap: {} > 2",
            state.eviction_buffer.len()
        );
        assert!(
            !state.pending_externalize_retry.is_empty(),
            "the failed writes spill into the retry list"
        );
    }
    assert!(first.externalized == 0, "no write can succeed: {first:?}");

    // Another pass while the store is still down: the retry is attempted,
    // fails again, and the buffer still never exceeds its cap.
    engine
        .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let second = engine.gc().await.unwrap();
    assert_eq!(second.externalized, 0);
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.len() <= 2,
            "repeated failure must not re-bloat the buffer"
        );
        assert_eq!(
            state.pending_externalize_retry.len(),
            first.evicted.saturating_sub(2),
            "the spill keeps every failed overflow item"
        );
    }
}

/// One hot entity with a huge bucket must not stage every descriptor before
/// the 32-row view limit cuts in.
#[tokio::test]
async fn external_view_stops_collecting_at_the_row_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "hot bucket").await;
    {
        let mut state = engine.state.lock().await;
        for i in 0..2_000 {
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("ref {i} under one hot entity"),
                agent_contracts::ContextKind::ToolObservation,
                agent_contracts::ContextScope::Session,
                agent_contracts::ContextRetention::Working,
                0.5,
                Some("tool:shell.exec".into()),
            );
            item.entities = vec!["hot-bucket".into()];
            item.scope_id = None;
            let context_ref = crate::store::make_context_ref(&item);
            state.external.push(crate::store::to_external_entry(
                &item,
                context_ref,
                1,
                1,
                None,
            ));
        }
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: "hot-bucket".into(),
        })
        .await
        .unwrap();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "hot-bucket".into(),
            budget_tokens: 10_000,
            hints: agent_contracts::ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized.external.len() <= 32,
        "the external view must not stage the whole bucket: {} rows",
        materialized.external.len()
    );
    assert!(
        (1..=32).contains(&materialized.external.len()),
        "the external view must stay within the 32-row surface without staging the bucket: {} rows",
        materialized.external.len()
    );
}

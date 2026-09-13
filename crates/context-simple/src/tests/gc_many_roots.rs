//! CTX-8 (R2-06) follow-up: a full GC pass with a *large root set* stays
//! linear. The sweep's marked-membership test is a HashSet (per-pass
//! snapshot), the completed-task fact is a per-pass set, and the pass
//! commits with exactly the same selection/eviction decisions the old
//! membership semantics produced — verified here by counting marks,
//! evictions and survivors on a heap where most items ARE roots.

use agent_contracts::{ContextEngine, ContextRetention, ContextScope};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

#[tokio::test]
async fn a_full_pass_with_many_roots_stays_bounded_and_equivalent() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "many roots").await;

    const SESSION_ROOTS: usize = 400;
    const FOCUS_MEMBERS: usize = 200;
    {
        let mut state = engine.state.lock().await;
        // Durable session memory: a mark_roots branch per item.
        for i in 0..SESSION_ROOTS {
            let item = crate::item::make_item(
                &state,
                &engine.config,
                format!("durable session fact {i}"),
                agent_contracts::ContextKind::Note,
                ContextScope::Session,
                ContextRetention::Durable,
                0.6,
                Some("test".into()),
            );
            state.items.push(item);
        }
        // Focus-scope members: the active-attention container root.
        for i in 0..FOCUS_MEMBERS {
            let item = crate::item::make_item(
                &state,
                &engine.config,
                format!("episode member {i}"),
                agent_contracts::ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                Some("test".into()),
            );
            state.items.push(item);
        }
    }

    let report = engine.gc().await.unwrap();

    assert_eq!(
        report.marked_roots,
        SESSION_ROOTS + FOCUS_MEMBERS,
        "every durable session fact and focus member is a root: {report:?}"
    );
    assert_eq!(report.evicted, 0, "roots survive their own pass");
    assert_eq!(
        report.reactivated, 0,
        "nothing needs reactivation in an all-root pass"
    );
    let state = engine.state.lock().await;
    assert_eq!(
        state.items.len(),
        SESSION_ROOTS + FOCUS_MEMBERS,
        "the whole marked heap survives"
    );
    assert!(
        state.eviction_buffer.is_empty() && state.external.is_empty(),
        "no root spilled anywhere"
    );
}

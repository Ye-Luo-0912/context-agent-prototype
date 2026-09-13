//! CTX-7 (R2-05): reading a stored body back must merge the *current owner
//! metadata* of the external entry over the blob's frozen snapshot. A
//! scope-close promotion re-stamps the entry, not the disk blob — so fetch,
//! admit and GC recall must not resurrect the stale pre-promotion (or
//! pre-terminal) metadata. Content and creation clocks are blob-immutable
//! and must survive every read unchanged.
//!
//! Every test walks the whole movement chain (externalize → promote → read
//! back through one of the three read paths → restore), not just the merge
//! helper on an isolated entry struct.

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextAction, ContextEngine, ContextHints,
    ContextIngress, ContextItemId, ContextKind, ContextMaterializationMissReason, ContextQuery,
    ContextResidency, ContextRetention, ContextScope, CoreLabel, Label, LifecycleLabel,
    ResourceKey, RootReason,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// Externalize one live Working item (with a promotable constraint tag and
/// a body the store keeps byte-exact) through the eviction-buffer overflow,
/// then promote it by closing its focus scope. Returns the item id, the
/// creation tick and the content captured before the trip.
async fn externalize_then_promote(engine: &SimpleContextEngine) -> (ContextItemId, u64, String) {
    let (item, focus_scope) = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "constraint body: cache eviction order is LRU".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.7,
            Some("test".into()),
        );
        item.scope_id = state.active_scope_id;
        item.tags.push(Label::core(CoreLabel::Constraint));
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        let focus_scope = state.active_scope_id.unwrap();
        state.eviction_buffer.push(item.clone());
        (item, focus_scope)
    };
    let id = item.id;
    let created_tick = item.created_tick;
    let content = item.content.clone();

    // The overflow goes straight to the store (buffer capacity 0).
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(id).is_some(),
            "the item must be externalized before the promotion can be staged"
        );
        assert!(
            state.eviction_buffer.iter().all(|item| item.id != id),
            "the item left the buffer"
        );
    }

    // Promote: close the focus scope; the stored entry is re-stamped to the
    // task scope, its retention upgrades Working -> Durable, the Promoted
    // label lands on the entry. The blob still holds the OLD snapshot.
    engine.close_scope(focus_scope).await.unwrap();
    {
        let state = engine.state.lock().await;
        let entry = state.external.get(id).unwrap();
        assert_eq!(
            entry.retention,
            ContextRetention::Durable,
            "setup: the entry must carry the promoted retention"
        );
        assert!(
            entry
                .tags
                .iter()
                .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
            "setup: the entry must carry the Promoted label"
        );
    }
    (id, created_tick, content)
}

fn store_engine() -> (SimpleContextEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0, // every eviction overflows straight to the store
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    (engine, dir)
}

#[tokio::test]
async fn fetch_returns_the_promoted_metadata_not_the_blob_snapshot() {
    let (engine, _dir) = store_engine();
    open_focus(&engine, "promote then fetch").await;
    let (id, created_tick, content) = externalize_then_promote(&engine).await;

    let fetched = engine
        .fetch_external(id)
        .await
        .unwrap()
        .expect("the stored body is retrievable");

    assert_eq!(fetched.content, content, "content is byte-identical");
    assert_eq!(
        fetched.created_tick, created_tick,
        "the creation clock never changes on a read"
    );
    assert_eq!(
        fetched.retention,
        ContextRetention::Durable,
        "the promoted retention survives the read back"
    );
    assert!(
        fetched
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
        "the promoted label survives the read back"
    );
}

#[tokio::test]
async fn admit_reenters_a_promoted_stored_body_with_the_promotion_intact() {
    let (engine, _dir) = store_engine();
    open_focus(&engine, "promote then admit").await;
    let (id, created_tick, content) = externalize_then_promote(&engine).await;

    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Admit {
                item_id: id,
                reason: "needed now".into(),
            },
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let admitted = state
        .items
        .iter()
        .find(|item| item.id == id)
        .expect("the admitted body is resident again");
    assert_eq!(admitted.content, content);
    assert_eq!(
        admitted.created_tick, created_tick,
        "admission never resets the creation clock (ADMIT-LEASE)"
    );
    assert_eq!(
        admitted.retention,
        ContextRetention::Durable,
        "the blob's stale Working retention must not override the promotion"
    );
    assert!(
        admitted
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
        "the Promoted label must survive the admit"
    );
}

#[tokio::test]
async fn gc_recall_lands_the_promoted_metadata_in_the_heap() {
    let (engine, _dir) = store_engine();
    open_focus(&engine, "promote then recall").await;
    let (id, created_tick, content) = externalize_then_promote(&engine).await;

    // A residency-strength anchor claim targeting the stored entry makes
    // the recall deterministic (claim-driven recalls have their own
    // budget). The recalled body must land with the entry's metadata.
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

    let state = engine.state.lock().await;
    let recalled = state
        .items
        .iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("the claim-held stored body is recalled: {report:?}"));
    assert_eq!(recalled.content, content);
    assert_eq!(recalled.created_tick, created_tick);
    assert_eq!(
        recalled.retention,
        ContextRetention::Durable,
        "the recalled body keeps the promoted retention, not the blob's stale one"
    );
    assert!(
        recalled
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted)),
        "the recalled body keeps the Promoted label"
    );
}

#[tokio::test]
async fn promoted_stored_metadata_survives_restore_and_the_next_fetch() {
    let (engine, _dir) = store_engine();
    open_focus(&engine, "promote, restore, continue").await;
    let (id, created_tick, content) = externalize_then_promote(&engine).await;

    // Checkpoint while the entry holds the promoted metadata; restore must
    // preserve it and the next fetch still merges correctly.
    let checkpoint = engine.checkpoint().await.unwrap();
    engine.restore(checkpoint).await.unwrap();

    let fetched = engine
        .fetch_external(id)
        .await
        .unwrap()
        .expect("the restored entry is retrievable");
    assert_eq!(fetched.content, content);
    assert_eq!(fetched.created_tick, created_tick);
    assert_eq!(fetched.retention, ContextRetention::Durable);
    assert!(
        fetched
            .tags
            .iter()
            .any(|tag| tag.is_lifecycle(LifecycleLabel::Promoted))
    );

    // The engine's own view still holds the promoted ownership metadata.
    let state = engine.state.lock().await;
    let entry = state.external.get(id).expect("the entry survives restore");
    assert_eq!(entry.retention, ContextRetention::Durable);
    assert_eq!(entry.item_id, id);
}

#[tokio::test]
async fn required_store_read_serves_the_promoted_metadata_not_the_blob_snapshot() {
    let (engine, _dir) = store_engine();
    open_focus(&engine, "promote then require").await;
    let (id, created_tick, content) = externalize_then_promote(&engine).await;

    // A PromptRequired claim forces the stored body through the required
    // plan's store-read path: the frame must carry the entry's promoted
    // metadata (Durable + Promoted), never the blob's stale Working
    // snapshot — the same merge rule fetch/admit/recall already obey.
    let materialized = engine
        .materialize(agent_contracts::ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![AnchorRootClaim {
                    item_ref: id.to_string(),
                    strength: AnchorRootStrength::PromptRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 2,
                    reason: RootReason::CompletionEvidence,
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    assert!(
        materialized
            .required_misses
            .as_slice()
            .iter()
            .all(|miss| miss.reason != ContextMaterializationMissReason::PolicyExcluded),
        "the live promoted entry must not be misjudged dead: {:?}",
        materialized.required_misses
    );
    let body = materialized
        .items
        .iter()
        .find(|entry| entry.item_id == id)
        .expect("the required stored body reaches the frame");
    assert_eq!(body.semantic, agent_contracts::SemanticState::Live);
    assert_eq!(
        body.retention,
        ContextRetention::Durable,
        "the required frame must carry the entry's promoted retention, not the blob's stale Working"
    );
    assert_eq!(body.content, content, "content is byte-identical");
    // The same merge rule keeps fetch consistent with the frame.
    let fetched = engine.fetch_external(id).await.unwrap().unwrap();
    assert_eq!(fetched.retention, ContextRetention::Durable);
    assert_eq!(fetched.created_tick, created_tick);
}

/// CTX-11 (R3-05/06): the same current file must project through every body
/// location. A Pending (store-write failed) body is foreground-projectable —
/// not "Missing" while `fetch` serves it — and a Stored read serves the
/// CURRENT owner metadata (promotion/retention), not the blob's frozen
/// snapshot. fetch and foreground agree on the same body.
#[tokio::test]
async fn foreground_projection_agrees_across_all_four_owner_locations() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        gc_buffer_capacity: 0, // evictions go straight to the store
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "edit the current files").await;

    // Pending copy: the store outage owns this in-memory body.
    let pending_id = {
        let mut state = engine.state.lock().await;
        let item = crate::item::make_item(
            &state,
            &engine.config,
            "     1 | pending body of src/a.rs".into(),
            ContextKind::ToolObservation,
            ContextScope::Task,
            ContextRetention::Ephemeral,
            0.4,
            Some("tool:fs.read".into()),
        );
        let id = item.id;
        let mut item = item;
        item.file_path = Some("src/a.rs".into());
        item.file_revision = Some("rev-1".into());
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        state.pending_externalize_retry.push(item);
        id
    };

    // R3-05 first, while the store outage still owns the body: the pending
    // file is foreground-projectable and fetch agrees (no GC has run, so
    // the body has not been drained into the store).
    let hints_a = ContextHints {
        foreground_resources: vec![ResourceKey {
            path: "src/a.rs".into(),
            revision: Some("rev-1".into()),
        }],
        ..ContextHints::default()
    };
    {
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "continue".into(),
                budget_tokens: 100_000,
                hints: hints_a,
            })
            .await
            .unwrap();
        assert!(
            materialized
                .required_misses
                .as_slice()
                .iter()
                .all(|miss| miss.reason != ContextMaterializationMissReason::Missing),
            "a pending current file must not read as Missing: {:?} fg={:?} items={:?}",
            materialized.required_misses,
            materialized.foreground,
            materialized.items
        );
        let projected = materialized
            .foreground
            .iter()
            .find(|item| item.item_id == pending_id)
            .expect("the pending body reaches the foreground");
        assert!(projected.semantic.is_live());
        let fetched = engine.fetch_external(pending_id).await.unwrap();
        assert!(
            fetched.is_none() || fetched.unwrap().semantic.is_live(),
            "fetch and foreground agree on the same live body"
        );
    }

    // Stored copy, with the OWNER metadata adjusted after externalization
    // (the promotion path re-stamps the entry, never the disk blob).
    let stored_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "     1 | stored body of src/b.rs".into(),
            ContextKind::ToolObservation,
            ContextScope::Task,
            ContextRetention::Working,
            0.4,
            Some("tool:fs.read".into()),
        );
        item.file_path = Some("src/b.rs".into());
        item.file_revision = Some("rev-1".into());
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        let id = item.id;
        state.eviction_buffer.push(item);
        id
    };
    engine.gc().await.unwrap();
    {
        let mut state = engine.state.lock().await;
        let entry = state.external.get_mut(stored_id).expect("stored");
        entry.retention = ContextRetention::Durable;
        entry.tags.push(Label::lifecycle(LifecycleLabel::Promoted));
    }

    let hints_b = ContextHints {
        foreground_resources: vec![ResourceKey {
            path: "src/b.rs".into(),
            revision: Some("rev-1".into()),
        }],
        ..ContextHints::default()
    };

    // R3-06: the Stored read serves the CURRENT owner metadata in the final
    // MaterializedContext (promoted Durable + Promoted label), not the
    // blob's stale Working snapshot.
    {
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "continue".into(),
                budget_tokens: 100_000,
                hints: hints_b,
            })
            .await
            .unwrap();
        let projected = materialized
            .foreground
            .iter()
            .find(|item| item.item_id == stored_id)
            .expect("the stored body reaches the foreground");
        assert_eq!(
            projected.retention,
            ContextRetention::Durable,
            "the foreground frame must carry the entry's adjusted retention"
        );
        assert!(projected.semantic.is_live());
        let fetched = engine.fetch_external(stored_id).await.unwrap().unwrap();
        assert_eq!(fetched.retention, ContextRetention::Durable);
    }
}

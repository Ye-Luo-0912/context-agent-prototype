//! B2 调用方完整性收口：hydration → GC/reconcile/search 的完整性传播。
//!
//! `hydrate_pending_cards_within_budget` 对一批读取全部失败时提前返回（避免无限重试），
//! 但调用方必须知道「仍有 pending 元数据未读取」。删除规划器的可达性闭包只
//! 遍历已安装条目的依赖边——pending 卡片里的出边不可见；只把 pending 的 id
//! 加入根集合不足以保护未读出的边。本模块钉住三条规则（同时写在
//! `hydrate_pending_cards_within_budget` 与 `plan_storage_gc` 上）：
//!
//! - pending owner ≠ ownerless；
//! - 恢复根完整 ≠ 元数据/依赖完整；
//! - 完整性未知时，不可逆删除延期。

use std::sync::atomic::Ordering;

use agent_contracts::{
    ContextEngine, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
    ContextSearchQuery, DependencyEdge, SemanticState,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

struct CitedEvidence {
    engine: SimpleContextEngine,
    /// The live citing record whose metadata is a pending card.
    citing: ContextItemId,
    /// The terminal evidence record the citing record strongly references.
    evidence: ContextItemId,
}

/// One live stored record A ("alpha …") with a strong `EvidenceFor` edge to
/// one terminal evidence record B that satisfies every storage-deletion
/// condition (dead, retention-eligible, aged past the storage TTL, no other
/// referencing root). A is left exactly as a bounded restore leaves it: its
/// metadata card is on disk, its `(id, card hash)` row sits in
/// `pending_external_cards`, and no entry is installed — while B is a live
/// map owner with its blob on the formal path.
async fn cited_evidence_engine() -> (tempfile::TempDir, CitedEvidence) {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "b2 hydration completeness").await;

    let (a_id, b_id, citing, evidence) = {
        let state = engine.state.lock().await;
        let evidence = crate::item::make_item(
            &state,
            &engine.config,
            "the stored evidence body".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("test".into()),
        );
        let mut citing = crate::item::make_item(
            &state,
            &engine.config,
            "alpha citing record keeps its evidence".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("test".into()),
        );
        citing
            .dependencies
            .push(DependencyEdge::evidence_for(evidence.id));
        (citing.id, evidence.id, citing, evidence)
    };
    // Externalize both through the real spill path (empty buffer capacity →
    // straight to the store), citing record first.
    let (mut citing, mut evidence) = (citing, evidence);
    citing.residency = ContextResidency::Warm;
    citing.evicted_at_tick = Some(0);
    evidence.residency = ContextResidency::Warm;
    evidence.evicted_at_tick = Some(0);
    engine.state.lock().await.eviction_buffer.push(citing);
    engine.gc().await.unwrap();
    engine.state.lock().await.eviction_buffer.push(evidence);
    engine.gc().await.unwrap();

    {
        let mut state = engine.state.lock().await;
        // B: a deletion candidate — dead, retention-eligible, aged past the
        // storage TTL, referenced by nothing the planner can see. A stays
        // deliberately live: a live record citing terminal evidence must
        // keep that evidence stored.
        let evidence_entry = state.external.get_mut(b_id).unwrap();
        evidence_entry.semantic = SemanticState::Superseded { by: None };
        evidence_entry.externalized_at_tick = 0;
        evidence_entry.residency = ContextResidency::External;

        // Spill A's metadata to a real card, then leave exactly the
        // post-restore state: card + pending row, no installed entry.
        let citing_entry = state.external.get(a_id).unwrap().clone();
        let bytes = crate::store::external_card_bytes(&citing_entry);
        let hash = crate::store::checksum_hex(&bytes)[..12].to_string();
        crate::store::write_external_card_async(
            &crate::store::store_dir(&engine.config),
            &crate::store::external_card_path(
                &crate::store::store_dir(&engine.config),
                a_id,
                &hash,
            ),
            &bytes,
        )
        .await
        .unwrap();
        let mut entries = state.external.take_all();
        entries.retain(|entry| entry.item_id != a_id);
        state.external.replace_all(entries);
        state.pending_external_cards.push((a_id, hash));
        state.sync_catalog();
        // Age the tick counter far past the storage TTL so B is deletable
        // the moment the planner cannot see the A→B edge.
        state.event_seq = engine.config.storage_ttl_ticks + 10;
    }

    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_external_cards.len(),
            1,
            "exactly the citing record is pending"
        );
        assert_eq!(state.pending_external_cards[0].0, a_id);
        assert!(state.external.get(a_id).is_none(), "A is not installed");
        assert!(state.external.get(b_id).is_some(), "B is a live map owner");
    }
    let cited = CitedEvidence {
        engine,
        citing: a_id,
        evidence: b_id,
    };
    (dir, cited)
}

/// B2 (red-first): the citing record's metadata is a pending card whose read
/// transiently fails. The old shape drained, saw "nothing installed", and
/// kept planning deletions against the partial external set: B, with no
/// visible referencing edge, was deleted as an orphan while its owner A was
/// still unread. The pass must defer instead, and the recovered pass must
/// page A in and keep both records.
#[tokio::test]
async fn an_unread_pending_citation_defers_storage_gc_deletion() {
    let (dir, cited) = cited_evidence_engine().await;
    let engine = &cited.engine;
    let (a_id, b_id) = (cited.citing, cited.evidence);
    // One transient I/O failure on A's card read.
    engine.card_read_failure_bomb.store(1, Ordering::Relaxed);

    let report = engine.storage_gc_protecting(&[], true).await.unwrap();
    assert_eq!(
        report.deleted, 0,
        "an unread pending owner must defer deletion, not orphan its evidence: {report:?}"
    );
    assert!(
        report
            .reasons
            .iter()
            .any(|row| row.contains("deletion deferred")),
        "the deferral must be reported honestly: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(b_id).is_some(),
            "the evidence keeps its entry"
        );
        assert_eq!(
            state.external_card_io_failures, 1,
            "the unread read is counted as an I/O failure, not hidden"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            1,
            "the row stays retryable (N02)"
        );
    }
    // The evidence body survives physically: B is terminal, so the model-
    // facing fetch refuses it by design (a dead entry is never served back);
    // storage GC's deletion target is the blob file plus the entry, and
    // both are still there.
    assert!(
        dir.path().join(format!("{b_id}.json")).exists(),
        "the evidence blob survives the deferred pass"
    );
    let evidence_body = crate::store::read_item(dir.path(), b_id)
        .expect("the evidence body still reads after the deferred pass");
    assert_eq!(evidence_body.content, "the stored evidence body");

    // Recovery: the disk answers again; the next pass pages A in and the
    // planner finally sees the A→B edge.
    let report = engine.storage_gc_protecting(&[], true).await.unwrap();
    assert_eq!(report.deleted, 0, "{report:?}");
    assert!(
        !report
            .reasons
            .iter()
            .any(|row| row.contains("deletion deferred")),
        "a completed hydration must not defer: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.pending_external_cards.is_empty(),
            "the recovered drain consumes the row"
        );
        assert!(
            state.external.get(a_id).is_some(),
            "A's metadata is readable again"
        );
        assert!(state.external.get(b_id).is_some());
    }
    let citing_body = engine
        .fetch_external(a_id)
        .await
        .unwrap()
        .expect("A's body fetches after hydration");
    assert!(
        citing_body.content.contains("alpha"),
        "the recovered body is the captured one: {:?}",
        citing_body
    );
}

/// B2 (red-first): the pending card's body matches the query, but its read
/// transiently fails. The old shape returned Ok(empty) — indistinguishable
/// from a complete zero-match over the full external set. It must fail
/// closed with the typed coverage error instead; after recovery the same
/// query hits the paged-in body.
#[tokio::test]
async fn an_unread_pending_page_is_never_a_complete_zero_match() {
    let (_dir, cited) = cited_evidence_engine().await;
    let engine = &cited.engine;
    let a_id = cited.citing;
    engine.card_read_failure_bomb.store(1, Ordering::Relaxed);

    let outcome = engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await;
    assert!(
        outcome.is_err(),
        "an unread pending page must not be reported as a complete zero-match; got {outcome:?}"
    );

    // Recovery: the drain pages A in and the same query now hits it.
    let hits = engine
        .search_external(ContextSearchQuery::new("alpha", 8))
        .await
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.item_id == a_id),
        "the recovered search hits the paged-in citing record: {hits:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        state.pending_external_cards.is_empty(),
        "the search drain consumed the recovered row"
    );
}

/// B2 (red-first): the pending card is its owner's only metadata on disk
/// (the blob is already gone — a crash window). A reconcile whose hydration
/// could not read that card must not let the orphan-card sweep delete it;
/// the recovered pass pages the owner back in from the surviving card.
#[tokio::test]
async fn an_unread_pending_owner_survives_reconcile() {
    let (dir, cited) = cited_evidence_engine().await;
    let engine = &cited.engine;
    let a_id = cited.citing;
    // The crash window: A's blob is gone; the pending card is the only
    // metadata left for the id.
    std::fs::remove_file(dir.path().join(format!("{a_id}.json"))).unwrap();
    engine.card_read_failure_bomb.store(1, Ordering::Relaxed);
    let card_survived = |dir: &std::path::Path, id: ContextItemId| {
        std::fs::read_dir(dir.join("cards"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry.file_name().into_string().is_ok_and(|name| {
                    name.starts_with(&format!("{id}.")) && name.ends_with(".card")
                })
            })
    };

    let report = engine.reconcile_store_protecting(&[], true).await.unwrap();
    assert!(
        card_survived(dir.path(), a_id),
        "an unread pending owner's card must not be swept as an orphan: {report:?}"
    );
    assert_eq!(
        engine.state.lock().await.pending_external_cards.len(),
        1,
        "the row stays retryable (N02)"
    );

    // Recovery: the card read lands and pages its owner back in; a later
    // reconcile with complete hydration keeps the card (its id is owned).
    let inspected = engine.inspect_external(a_id).await.unwrap();
    assert!(
        inspected.is_some(),
        "the recovered card pages its owner back in"
    );
    {
        let state = engine.state.lock().await;
        assert!(state.pending_external_cards.is_empty());
        assert!(state.external.get(a_id).is_some());
    }
    let report = engine.reconcile_store_protecting(&[], true).await.unwrap();
    assert_eq!(
        report.external_cards_removed, 0,
        "the recovered pass owns the card's id and keeps it: {report:?}"
    );
}

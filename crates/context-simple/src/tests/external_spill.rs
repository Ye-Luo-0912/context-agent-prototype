//! CTX-9 残余：checkpoint 外置尾分片——最旧的超额 External 条目把元数据
//! 卡片写进既有 store（内容寻址、幂等），checkpoint 只携带内联段＋
//! `external_spilled` 寻址清单；restore 从卡片全量重水化。搜索/召回/
//! 目录语义不变（条目全部驻留内存，分片只压缩 checkpoint 字节）。
//! 卡片缺失/损坏 = 恢复的 external 集合不完整，如实计数、整体不失败。

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextQuery, ContextResidency, ContextRetention,
    ContextScope,
};

use crate::checkpoint;
use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

fn spill_config(dir: &tempfile::TempDir, inline_target: usize) -> SimpleContextConfig {
    SimpleContextConfig {
        external_checkpoint_inline_target: inline_target,
        external_checkpoint_card_batch: 64,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

async fn spill_engine(dir: &tempfile::TempDir, inline_target: usize) -> SimpleContextEngine {
    SimpleContextEngine::new(spill_config(dir, inline_target))
}

/// Manifest rows of one capture, in manifest order.
fn manifest_ids(value: &serde_json::Value) -> Vec<agent_contracts::ContextItemId> {
    value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    agent_contracts::ContextItemId::parse_ref(row["id"].as_str().unwrap()).unwrap()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn inline_len(value: &serde_json::Value) -> usize {
    value
        .get("external")
        .and_then(|v| v.as_array())
        .map(|rows| rows.len())
        .unwrap_or(0)
}

fn card_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir.join("cards")) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "card"))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Externalize `n` items (empty eviction buffer → straight to the store) and
/// pin their residency at External so the spill gate sees them.
async fn externalize_n(
    engine: &SimpleContextEngine,
    n: usize,
) -> Vec<agent_contracts::ContextItemId> {
    let mut ids = Vec::new();
    for index in 0..n {
        {
            let mut state = engine.state.lock().await;
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                format!("spill body {index} unique-token-{index}"),
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                Some("spill-test".into()),
            );
            item.residency = ContextResidency::Warm;
            item.evicted_at_tick = Some(0);
            let id = item.id;
            state.eviction_buffer.push(item);
            ids.push(id);
        }
        engine.gc().await.unwrap();
    }
    {
        let mut state = engine.state.lock().await;
        for id in &ids {
            let entry = state.external.get_mut(*id).expect("externalized");
            entry.residency = ContextResidency::External;
        }
    }
    ids
}

#[tokio::test]
async fn checkpoint_external_tail_is_spilled_and_restores_identically() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill the external tail").await;
    let ids = externalize_n(&engine, 30).await;

    let value = engine.checkpoint().await.unwrap();
    let spilled = value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .expect("the beyond-target tail must be spilled")
        .clone();
    assert_eq!(spilled.len(), 20, "the oldest 20 entries are spilled");
    let inline_len = value
        .get("external")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .expect("inline entries remain an array");
    assert_eq!(inline_len, 10, "the newest 10 entries stay inline");
    // 最旧优先：被分片的恰是先外置的前 20 个 id。
    let spilled_ids: Vec<_> = spilled
        .iter()
        .map(|item| {
            agent_contracts::ContextItemId::parse_ref(item["id"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(spilled_ids, ids[..20]);

    // 内容寻址幂等：第二次 capture 不新增卡片文件。
    let card_count = |dir: &std::path::Path| {
        std::fs::read_dir(dir.join("cards"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "card"))
            .count()
    };
    assert_eq!(card_count(dir.path()), 20, "one card per spilled entry");
    let _ = engine.checkpoint().await.unwrap();
    assert_eq!(
        card_count(dir.path()),
        20,
        "an unchanged tail re-hits the same content-addressed cards"
    );

    // restore 全量重水化：30 个条目一个不少，驻留与摘要一致。
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.external.len(), 30);
        for id in &ids {
            let entry = state.external.get(*id).expect("rehydrated");
            assert_eq!(entry.residency, ContextResidency::External);
        }
        assert_eq!(state.external_cards_missing, 0);
    }
}

#[tokio::test]
async fn a_mutated_spilled_entry_reflects_capture_time_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill then mutate").await;
    let ids = externalize_n(&engine, 15).await;

    let first = engine.checkpoint().await.unwrap();
    // 变更一个已分片条目的非索引字段（访问戳）。
    let mutated = ids[0];
    {
        let mut state = engine.state.lock().await;
        state.external.get_mut(mutated).unwrap().last_access_tick = 4242;
    }
    let second = engine.checkpoint().await.unwrap();
    engine.restore(second).await.unwrap();
    let state = engine.state.lock().await;
    assert_eq!(
        state.external.get(mutated).unwrap().last_access_tick,
        4242,
        "the card must carry CAPTURE-time metadata, not spill-time"
    );
    let _ = first;
}

#[tokio::test]
async fn a_missing_card_degrades_the_restore_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill then lose a card").await;
    let ids = externalize_n(&engine, 15).await;
    let value = engine.checkpoint().await.unwrap();
    let spilled: Vec<agent_contracts::ContextItemId> = value["external_spilled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            agent_contracts::ContextItemId::parse_ref(item["id"].as_str().unwrap()).unwrap()
        })
        .collect();
    let lost = spilled[0];

    // 删除一张卡片（典型：已过保留窗的旧 checkpoint，其条目随后被
    // Storage GC 清理）。
    let card = std::fs::read_dir(dir.path().join("cards"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(&format!("{lost}.")) && name.ends_with(".card")
                })
        })
        .expect("the lost entry's card exists")
        .path();
    std::fs::remove_file(&card).unwrap();

    engine.restore(value).await.unwrap();
    let state = engine.state.lock().await;
    assert!(
        state.external.get(lost).is_none(),
        "the entry whose card is gone is honestly absent"
    );
    assert_eq!(state.external_cards_missing, 1);
    assert_eq!(state.external.len(), 14, "every other entry rehydrates");
    for id in &ids[1..] {
        if *id != lost {
            assert!(state.external.get(*id).is_some());
        }
    }
}

#[tokio::test]
async fn recovery_roots_cover_spilled_ids() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill and protect").await;
    let ids = externalize_n(&engine, 20).await;
    let value = engine.checkpoint().await.unwrap();

    let roots = checkpoint::recovery_item_ids(&value).expect("valid spill list");
    for id in &ids {
        assert!(
            roots.contains(id),
            "a spilled entry's blob must stay a protected recovery root: {id}"
        );
    }
}

#[tokio::test]
async fn reconcile_cleans_orphan_cards_and_honors_protection() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill then reconcile").await;
    let _ids = externalize_n(&engine, 30).await;
    let value = engine.checkpoint().await.unwrap();
    let spilled: Vec<agent_contracts::ContextItemId> = value["external_spilled"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            agent_contracts::ContextItemId::parse_ref(item["id"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(spilled.len(), 20);

    // N01: 卡片的删除许可与 blob 完全一致。前 9 个分片条目模拟真实
    // Storage GC 孤儿（GC 只删 blob 不删卡片——blob 与 map 条目都消失）；
    // spilled[9] 只删 map 条目、保留 blob——该 blob 会被同一次扫描重新
    // 认领（rebuilt），其卡片必须一起幸存（N01 旧行为反例：卡片先于
    // owner 提交被当孤儿删除）。
    let removed: std::collections::HashSet<agent_contracts::ContextItemId> =
        spilled[..10].iter().copied().collect();
    let rebuilt_source = spilled[9];
    {
        let mut state = engine.state.lock().await;
        state
            .external
            .retain(|entry| !removed.contains(&entry.item_id));
    }
    for id in removed.iter().filter(|id| **id != rebuilt_source) {
        std::fs::remove_file(dir.path().join(format!("{id}.json"))).unwrap();
    }
    // 保留 checkpoint 的恢复承诺仍覆盖第一个被删 id：其卡片必须幸存。
    let protected = vec![spilled[0]];
    let (map_checksums, resident_ids) = {
        let state = engine.state.lock().await;
        (
            state
                .external
                .iter()
                .map(|entry| (entry.item_id, entry.blob_checksum.clone()))
                .collect::<std::collections::HashMap<_, _>>(),
            std::collections::HashSet::new(),
        )
    };
    let io = crate::store::run_reconcile_io_protecting(
        dir.path(),
        &map_checksums,
        &resident_ids,
        &protected,
        true,
    )
    .await;
    assert_eq!(
        io.external_cards_removed, 8,
        "only true orphans (blob and owner both gone, unprotected) go: {:?}",
        io.reasons
    );
    assert_eq!(
        io.rebuilt_candidates.len(),
        1,
        "the surviving ownerless blob is re-claimed this scan"
    );
    let cards_dir = dir.path().join("cards");
    let card_names: Vec<String> = std::fs::read_dir(&cards_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(
        card_names.len(),
        12,
        "10 mapped + 1 protected + 1 rebuilt: {:?}",
        card_names
    );
    assert!(
        card_names
            .iter()
            .any(|name| name.starts_with(&spilled[0].to_string())),
        "the protected card survives"
    );
    assert!(
        card_names
            .iter()
            .any(|name| name.starts_with(&rebuilt_source.to_string())),
        "the rebuilt id's card survives with its blob (N01)"
    );
}

/// N01 (W03): with the recovery-root enumeration incomplete, "absent from
/// the known protected set" proves nothing — card deletion defers exactly
/// like the blob sweep's stale-duplicate branch. The same fresh-engine
/// state under a complete-root pass is what may delete: there the
/// ownerless ids' blobs are re-claimed (rebuilt), so their cards survive
/// WITH them.
#[tokio::test]
async fn an_incomplete_root_enumeration_defers_card_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill then defer").await;
    let _ids = externalize_n(&engine, 12).await;
    let value = engine.checkpoint().await.unwrap();
    assert_eq!(value["external_spilled"].as_array().unwrap().len(), 2);

    // Fresh-engine shape: restore ownership not installed — an empty
    // external map with two cards on disk the map does not own.
    {
        let mut state = engine.state.lock().await;
        state.external.retain(|_| false);
    }
    let io = crate::store::run_reconcile_io_protecting(
        dir.path(),
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
        &[],
        false,
    )
    .await;
    assert_eq!(
        io.external_cards_removed, 0,
        "an incomplete root set must defer every card deletion: {:?}",
        io.reasons
    );
    assert_eq!(
        card_files(dir.path()).len(),
        2,
        "both cards survive the incomplete sweep"
    );

    // The complete-root pass over the identical state: every ownerless
    // blob is re-claimed (rebuilt), so every card survives with its owner.
    let io_complete = crate::store::run_reconcile_io_protecting(
        dir.path(),
        &std::collections::HashMap::new(),
        &std::collections::HashSet::new(),
        &[],
        true,
    )
    .await;
    assert_eq!(
        io_complete.rebuilt_candidates.len(),
        12,
        "the complete pass re-claims every ownerless blob (2 spilled + 10 inline)"
    );
    assert_eq!(
        card_files(dir.path()).len(),
        2,
        "the rebuilt pairs keep their cards"
    );
}

// ---------------------------------------------------------------------------
// F2: the capture's work is bounded and happens off the state lock, and a
// restore does not have to load the whole cold tail before the next turn.
// ---------------------------------------------------------------------------

/// Deterministic barrier, no timing inference: park a capture exactly at its
/// card-write boundary and prove an unrelated state read answers *while the
/// writes are parked*. The timeout is a deadlock guard; the assertion is that
/// diagnostics returns at all before the release fires.
#[tokio::test]
async fn capture_card_writes_run_with_the_state_lock_released() {
    let dir = tempfile::tempdir().unwrap();
    let engine = std::sync::Arc::new(spill_engine(&dir, 10).await);
    open_focus(&engine, "prove the capture lock boundary").await;
    externalize_n(&engine, 15).await;

    let planned = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    *engine
        .checkpoint_io_pause
        .lock()
        .expect("checkpoint test pause mutex poisoned") = Some(crate::engine::IoBoundaryPause {
        planned: std::sync::Arc::clone(&planned),
        release: std::sync::Arc::clone(&release),
    });

    let capture = {
        let engine = std::sync::Arc::clone(&engine);
        tokio::spawn(async move { engine.checkpoint().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), planned.notified())
        .await
        .expect("the capture reached its card-write boundary");

    let diagnostics = tokio::time::timeout(std::time::Duration::from_secs(10), {
        let engine = std::sync::Arc::clone(&engine);
        async move { engine.diagnostics().await }
    })
    .await
    .expect("diagnostics must not queue behind the capture's card writes")
    .unwrap();
    assert!(
        diagnostics.total_items >= 15,
        "the parked capture answered from a live state: {diagnostics:?}"
    );

    release.notify_one();
    let value = capture.await.unwrap().unwrap();
    assert_eq!(
        manifest_ids(&value).len(),
        5,
        "the released capture still spilled the over-target tail"
    );
    *engine
        .checkpoint_io_pause
        .lock()
        .expect("checkpoint test pause mutex poisoned") = None;
}

/// A tail nobody touched is spilled from the recorded-card directory: one
/// hash lookup per entry, no serialization and no write. The starved engine
/// has *zero* write, byte and serialization budget, so every manifest row it
/// produces can only come from that directory.
#[tokio::test]
async fn an_unchanged_tail_respills_without_reserializing_it() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "spill once, then re-spill for free").await;
    externalize_n(&engine, 30).await;

    let first = engine.checkpoint().await.unwrap();
    assert_eq!(manifest_ids(&first).len(), 20);
    let cards_after_first = card_files(dir.path()).len();
    assert_eq!(cards_after_first, 20);

    let starved = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_card_batch: 0,
        external_checkpoint_card_bytes: 0,
        external_checkpoint_scan_budget: 0,
        ..spill_config(&dir, 10)
    });
    starved.restore(first).await.unwrap();
    let second = starved.checkpoint().await.unwrap();

    assert_eq!(
        manifest_ids(&second).len(),
        20,
        "an unchanged tail stays spilled without any serialization budget"
    );
    assert_eq!(inline_len(&second), 10);
    assert_eq!(
        card_files(dir.path()).len(),
        cards_after_first,
        "no card is rewritten for an unchanged entry"
    );
}

/// The directory only ever claims what is really on disk: handing out a
/// mutable entry drops its row, so the next capture serializes that entry
/// again and the card it writes describes the new metadata.
#[tokio::test]
async fn a_mutated_entry_loses_its_recorded_card_and_is_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "mutate a spilled entry").await;
    let ids = externalize_n(&engine, 15).await;

    let first = engine.checkpoint().await.unwrap();
    assert_eq!(manifest_ids(&first).len(), 5);
    assert_eq!(engine.state.lock().await.external.recorded_cards(), 5);

    {
        let mut state = engine.state.lock().await;
        state.external.get_mut(ids[0]).unwrap().last_access_tick = 4242;
    }
    assert_eq!(
        engine.state.lock().await.external.recorded_cards(),
        4,
        "a mutable handle drops the card claim for that entry"
    );

    let second = engine.checkpoint().await.unwrap();
    assert_eq!(manifest_ids(&second).len(), 5);
    assert_eq!(
        card_files(dir.path()).len(),
        6,
        "only the mutated entry is serialized again"
    );
}

/// The serialization budget bounds one capture's lock-held work; because
/// already-carded entries are free, successive captures converge on the whole
/// over-target tail instead of stalling at the first window.
#[tokio::test]
async fn a_capture_serialization_budget_bounds_one_pass_and_converges() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_scan_budget: 5,
        ..spill_config(&dir, 10)
    });
    open_focus(&engine, "bound one capture").await;
    externalize_n(&engine, 30).await;

    let mut manifest_sizes = Vec::new();
    for _ in 0..4 {
        let value = engine.checkpoint().await.unwrap();
        manifest_sizes.push(manifest_ids(&value).len());
    }
    assert_eq!(
        manifest_sizes,
        vec![5, 10, 15, 20],
        "each capture serializes at most the budget, and the tail converges"
    );
    assert_eq!(card_files(dir.path()).len(), 20);

    let converged = engine.checkpoint().await.unwrap();
    assert_eq!(inline_len(&converged), 10);
    engine.restore(converged).await.unwrap();
    let state = engine.state.lock().await;
    assert_eq!(state.external.len(), 30);
    assert_eq!(state.external_cards_missing, 0);
}

/// A restore reads one bounded batch of cards and leaves the rest as
/// `(id, card hash)` rows: the next turn's prompt assembly needs no cold
/// metadata, an id lookup pages in its own card, and search still sees the
/// complete external set.
#[tokio::test]
async fn a_restore_pages_in_a_bounded_batch_and_defers_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "restore without the whole tail").await;
    let ids = externalize_n(&engine, 30).await;
    let value = engine.checkpoint().await.unwrap();
    let spilled = manifest_ids(&value);
    assert_eq!(spilled.len(), 20);

    let restored = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 5,
        ..spill_config(&dir, 10)
    });
    restored.restore(value).await.unwrap();
    {
        let state = restored.state.lock().await;
        assert_eq!(
            state.external.len(),
            15,
            "the inline entries plus exactly one bounded batch of cards"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            15,
            "the rest of the tail is a directory of (id, card hash) rows"
        );
    }
    let total_while_pending = restored.diagnostics().await.unwrap().total_items;

    let preview = restored
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4_096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert!(preview.materialization_id > 0);
    assert_eq!(
        restored.state.lock().await.pending_external_cards.len(),
        15,
        "the next turn's materialization reads no cold metadata"
    );

    // An id lookup works the moment the restore returns.
    let deferred = spilled[19];
    let fetched = restored.fetch_external(deferred).await.unwrap();
    assert!(
        fetched.is_some(),
        "a deferred row is still retrievable by id"
    );
    {
        let state = restored.state.lock().await;
        assert_eq!(state.pending_external_cards.len(), 14);
        assert!(state.external.get(deferred).is_some());
    }

    // Search coverage is unchanged: the directory drains in bounded batches
    // before candidates are generated.
    let hits = restored
        .search_external(agent_contracts::ContextSearchQuery::new(
            "unique-token-19",
            8,
        ))
        .await
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.item_id == ids[19]),
        "a spilled entry is searchable again after the drain"
    );
    {
        let state = restored.state.lock().await;
        assert!(
            state.pending_external_cards.is_empty(),
            "search drains the pending directory"
        );
        assert_eq!(state.external.len(), 30);
        assert_eq!(state.external_cards_missing, 0);
    }
    assert_eq!(
        restored.diagnostics().await.unwrap().total_items,
        total_while_pending,
        "the logical total never dipped while rows were pending"
    );

    // Paging kept the map in externalization order, so the next capture's
    // oldest-first choice is the same tail as before the restore.
    let recaptured = restored.checkpoint().await.unwrap();
    assert_eq!(
        manifest_ids(&recaptured),
        spilled,
        "a paged-in entry lands in externalization order, not at the end"
    );
}

/// A capture taken before any paging still names every deferred row, so a
/// bounded restore followed by a checkpoint cannot lose the tail.
#[tokio::test]
async fn a_capture_taken_before_paging_keeps_every_deferred_row() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "capture before paging").await;
    externalize_n(&engine, 30).await;
    let value = engine.checkpoint().await.unwrap();

    let restored = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 0,
        ..spill_config(&dir, 10)
    });
    restored.restore(value).await.unwrap();
    {
        let state = restored.state.lock().await;
        assert_eq!(state.pending_external_cards.len(), 20);
        assert_eq!(state.external.len(), 10);
    }

    let second = restored.checkpoint().await.unwrap();
    assert_eq!(
        manifest_ids(&second).len(),
        20,
        "a capture re-emits the rows it has not paged in"
    );
    assert_eq!(inline_len(&second), 10);

    let reloaded = spill_engine(&dir, 10).await;
    reloaded.restore(second).await.unwrap();
    let state = reloaded.state.lock().await;
    assert_eq!(
        state.external.len(),
        30,
        "every entry survives the round trip"
    );
    assert_eq!(state.external_cards_missing, 0);
}

/// A pending row is a live owner whose metadata is not in memory. The
/// reconcile sweep must page it in before deciding what is orphaned —
/// otherwise a bounded restore would hand every deferred body to the
/// stale-blob deletion branch.
#[tokio::test]
async fn a_deferred_restore_row_is_never_reclaimed_as_an_orphan() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "reconcile after a bounded restore").await;
    let ids = externalize_n(&engine, 30).await;
    let value = engine.checkpoint().await.unwrap();

    let restored = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 0,
        ..spill_config(&dir, 10)
    });
    restored.restore(value).await.unwrap();
    assert_eq!(restored.state.lock().await.pending_external_cards.len(), 20);

    let report = restored.reconcile_store().await.unwrap();
    assert_eq!(
        report.deleted_stale, 0,
        "a pending row still owns its blob: {:?}",
        report.reasons
    );
    assert_eq!(
        report.external_cards_removed, 0,
        "a pending row's card is not an orphan: {:?}",
        report.reasons
    );
    {
        let state = restored.state.lock().await;
        assert_eq!(
            state.external.len(),
            30,
            "the sweep paged the directory in before classifying blobs"
        );
        assert!(state.pending_external_cards.is_empty());
    }
    for id in [ids[0], ids[19], ids[29]] {
        assert!(
            restored.fetch_external(id).await.unwrap().is_some(),
            "every body stays readable by id: {id}"
        );
    }
}

// ---------------------------------------------------------------------------
// F1: a hostile or malformed spill restore must fail closed without
// replacing live state. Kept alongside F2's bounded paging.
// ---------------------------------------------------------------------------

const LIVE_MARKER: &str = "LIVE-MARKER-MUST-SURVIVE";

async fn engine_with_live_marker() -> SimpleContextEngine {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "live state must survive a refused restore").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: LIVE_MARKER.into(),
        })
        .await
        .unwrap();
    engine
}

async fn assert_live_marker_untouched(engine: &SimpleContextEngine, before: &serde_json::Value) {
    let after = engine.checkpoint().await.unwrap();
    assert_eq!(
        after, *before,
        "a refused restore must leave the live checkpoint byte-identical"
    );
    let state = engine.state.lock().await;
    assert!(
        state
            .items
            .iter()
            .any(|item| item.content.contains(LIVE_MARKER)),
        "the live marker heap item must still be present"
    );
    assert_eq!(
        state.external_cards_missing, 0,
        "a structural reject must not take the missing-card degrade path"
    );
}

fn colliding_owner_checkpoint(
    location: CollidingOwner,
) -> (serde_json::Value, agent_contracts::ContextItemId) {
    let config = SimpleContextConfig::default();
    let mut state = crate::engine::State::default();
    let mut item = crate::item::make_item(
        &state,
        &config,
        "body that already owns this id".into(),
        ContextKind::Note,
        ContextScope::Task,
        ContextRetention::Working,
        0.5,
        Some("spill-f1".into()),
    );
    item.residency = match location {
        CollidingOwner::Warm => ContextResidency::Warm,
        CollidingOwner::Pending => ContextResidency::Warm,
    };
    item.evicted_at_tick = Some(0);
    let id = item.id;
    match location {
        CollidingOwner::Warm => state.eviction_buffer.push(item),
        CollidingOwner::Pending => state.pending_externalize_retry.push(item),
    }
    state.sync_catalog();
    let mut value = checkpoint::serialize(&state).unwrap();
    value["external_spilled"] = serde_json::json!([{
        "id": id.to_string(),
        "hash": "0123456789ab",
    }]);
    (value, id)
}

#[derive(Clone, Copy)]
enum CollidingOwner {
    Warm,
    Pending,
}

/// Review anti-example: a valid spilled checkpoint with one `external_spilled`
/// row duplicated must be refused, and must not replace live state.
#[tokio::test]
async fn duplicate_spill_id_in_manifest_is_refused_without_mutating_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "duplicate spill id").await;
    let ids = externalize_n(&engine, 15).await;
    let mut hostile = engine.checkpoint().await.unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: LIVE_MARKER.into(),
        })
        .await
        .unwrap();
    let before = engine.checkpoint().await.unwrap();

    let first = hostile["external_spilled"]
        .as_array()
        .expect("the tail was spilled")
        .first()
        .expect("at least one spilled row")
        .clone();
    hostile["external_spilled"]
        .as_array_mut()
        .unwrap()
        .push(first);

    let error = engine.restore(hostile).await.unwrap_err().to_string();
    assert!(
        error.contains("both spilled and already owned"),
        "duplicate spill ids are a structural contradiction, got: {error}"
    );
    assert_eq!(
        engine.checkpoint().await.unwrap(),
        before,
        "duplicate spill restore must not mutate live state"
    );
    let state = engine.state.lock().await;
    assert!(
        state
            .items
            .iter()
            .any(|item| item.content.contains(LIVE_MARKER)),
        "the post-checkpoint live marker must survive"
    );
    assert_eq!(state.external.len(), 15);
    for id in &ids {
        assert!(state.external.get(*id).is_some());
    }
    assert_eq!(state.external_cards_missing, 0);
}

#[tokio::test]
async fn spill_id_colliding_with_warm_buffer_is_refused_without_mutating_live_state() {
    let engine = engine_with_live_marker().await;
    let before = engine.checkpoint().await.unwrap();
    let (hostile, id) = colliding_owner_checkpoint(CollidingOwner::Warm);
    let error = engine.restore(hostile).await.unwrap_err().to_string();
    assert!(
        error.contains(&id.to_string()) && error.contains("both spilled and already owned"),
        "Warm collision must fail closed, got: {error}"
    );
    assert_live_marker_untouched(&engine, &before).await;
}

#[tokio::test]
async fn spill_id_colliding_with_pending_retry_is_refused_without_mutating_live_state() {
    let engine = engine_with_live_marker().await;
    let before = engine.checkpoint().await.unwrap();
    let (hostile, id) = colliding_owner_checkpoint(CollidingOwner::Pending);
    let error = engine.restore(hostile).await.unwrap_err().to_string();
    assert!(
        error.contains(&id.to_string()) && error.contains("both spilled and already owned"),
        "Pending collision must fail closed, got: {error}"
    );
    assert_live_marker_untouched(&engine, &before).await;
}

/// Present-but-invalid spill rows are structural errors: they must not be
/// silently dropped (which would let restore succeed and clobber live state).
#[tokio::test]
async fn illegal_spill_manifest_row_is_refused_without_mutating_live_state() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "illegal spill row").await;
    let _ids = externalize_n(&engine, 15).await;
    let mut hostile = engine.checkpoint().await.unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: LIVE_MARKER.into(),
        })
        .await
        .unwrap();
    let before = engine.checkpoint().await.unwrap();

    hostile["external_spilled"]
        .as_array_mut()
        .expect("the tail was spilled")
        .push(serde_json::json!({
            "id": "not-a-uuid",
            "hash": "0123456789ab",
        }));

    let error = engine.restore(hostile).await.unwrap_err().to_string();
    assert!(
        error.contains("checkpoint restore validation") && error.contains("external_spilled"),
        "an illegal present row must fail closed, got: {error}"
    );
    assert_eq!(
        engine.checkpoint().await.unwrap(),
        before,
        "illegal spill restore must not mutate live state"
    );
    let state = engine.state.lock().await;
    assert!(
        state
            .items
            .iter()
            .any(|item| item.content.contains(LIVE_MARKER))
    );
    assert_eq!(state.external_cards_missing, 0);
}

// ---------------------------------------------------------------------------
// N02/N03: 冷页读取的取消安全、瞬态故障可重试、以及卡片的资源/一致性边界。
// ---------------------------------------------------------------------------

/// One engine captured three spilled cards; a fresh engine restores it with
/// `external_restore_card_batch: 1`, leaving two pending rows — the exact
/// production shape of a sharded restore's deferred tail. Returns the engine
/// and the two deferred ids.
async fn sharded_restore_with_pending_tail(
    dir: &tempfile::TempDir,
) -> (
    SimpleContextEngine,
    [agent_contracts::ContextItemId; 2],
) {
    let first = spill_engine(dir, 10).await;
    open_focus(&first, "sharded restore tail").await;
    let _ids = externalize_n(&first, 13).await;
    let value = first.checkpoint().await.unwrap();
    assert_eq!(manifest_ids(&value).len(), 3);

    let second = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 1,
        ..spill_config(dir, 10)
    });
    second.restore(value).await.unwrap();
    let pending = second.state.lock().await.pending_external_cards.clone();
    assert_eq!(pending.len(), 2, "the batch bound defers two rows");
    (second, [pending[0].0, pending[1].0])
}

/// N02 (red-first): a future dropped mid-batch — parked deterministically at
/// the second card's read boundary and aborted — must not lose a single
/// pending row. The old shape moved the rows out of the queue before the
/// reads: the abort consumed them, and the next checkpoint's manifest
/// silently dropped their ids.
#[tokio::test]
async fn a_cancelled_batch_hydration_keeps_every_pending_row() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _pending_ids) = sharded_restore_with_pending_tail(&dir).await;
    assert_eq!(engine.state.lock().await.pending_external_cards.len(), 2);

    let planned = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    *engine
        .card_read_pause
        .lock()
        .expect("card read pause mutex poisoned") = Some((
        crate::engine::IoBoundaryPause {
            planned: std::sync::Arc::clone(&planned),
            release: std::sync::Arc::clone(&release),
        },
        1,
    ));

    let engine = std::sync::Arc::new(engine);
    let batch = {
        let engine = std::sync::Arc::clone(&engine);
        tokio::spawn(async move { engine.hydrate_pending_cards(2).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), planned.notified())
        .await
        .expect("the hydration reached the second card's read boundary");
    batch.abort();
    release.notify_one();
    let _ = batch.await;

    assert_eq!(
        engine.state.lock().await.pending_external_cards.len(),
        2,
        "an aborted hydration must not consume a single pending row"
    );

    // A later, undisturbed drain installs everything: nothing was lost.
    *engine.card_read_pause.lock().expect("poisoned") = None;
    let installed = engine.hydrate_pending_cards(2).await;
    assert_eq!(installed, 2, "the retry installs every deferred row");
    let state = engine.state.lock().await;
    assert_eq!(state.pending_external_cards.len(), 0);
    assert_eq!(state.external_cards_missing, 0);
}

/// N02 (red-first): cancelling a single-id fetch mid-read keeps its pending
/// row, so the next fetch still resolves the body. The old shape removed the
/// row before the read: the aborted fetch consumed the only locator and the
/// body became unreachable.
#[tokio::test]
async fn a_cancelled_id_fetch_keeps_its_pending_row() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _pending_ids) = sharded_restore_with_pending_tail(&dir).await;
    let target = engine.state.lock().await.pending_external_cards[0].0;

    let planned = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    *engine
        .card_read_pause
        .lock()
        .expect("card read pause mutex poisoned") = Some((
        crate::engine::IoBoundaryPause {
            planned: std::sync::Arc::clone(&planned),
            release: std::sync::Arc::clone(&release),
        },
        0,
    ));

    let engine = std::sync::Arc::new(engine);
    let fetch = {
        let engine = std::sync::Arc::clone(&engine);
        tokio::spawn(async move { engine.fetch_external(target).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), planned.notified())
        .await
        .expect("the fetch reached its card-read boundary");
    fetch.abort();
    release.notify_one();
    let _ = fetch.await;

    assert!(
        engine
            .state
            .lock()
            .await
            .pending_external_cards
            .iter()
            .any(|(id, _)| *id == target),
        "an aborted fetch must keep the pending locator"
    );

    *engine.card_read_pause.lock().expect("poisoned") = None;
    let item = engine
        .fetch_external(target)
        .await
        .unwrap()
        .expect("the retried fetch pages the card in and serves the body");
    assert!(
        item.content.contains("unique-token"),
        "the fetched body is the captured one: {}",
        item.content
    );
    assert_eq!(
        engine.state.lock().await.pending_external_cards.len(),
        1,
        "only the fetched row left the queue"
    );
}

/// N02 (red-first): one transient I/O failure keeps the retryable locator
/// and counts as an I/O failure — never as "the data does not exist". The
/// next drain, with the disk recovered, installs the entry exactly once.
#[tokio::test]
async fn a_transient_card_read_failure_keeps_the_retryable_locator() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, _pending_ids) = sharded_restore_with_pending_tail(&dir).await;
    engine
        .card_read_failure_bomb
        .store(1, std::sync::atomic::Ordering::Relaxed);

    let installed = engine.hydrate_pending_cards(2).await;
    assert_eq!(
        installed, 1,
        "only the failed read installs nothing; the other row is fine"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_external_cards.len(),
            1,
            "a transient failure must not consume the locator"
        );
        assert_eq!(
            state.external_card_io_failures, 1,
            "the transient failure is counted, separately from missing"
        );
        assert_eq!(
            state.external_cards_missing, 0,
            "a transient failure is never counted as absent data"
        );
    }

    let installed_again = engine.hydrate_pending_cards(1).await;
    assert_eq!(installed_again, 1, "the recovered read installs the row");
    let state = engine.state.lock().await;
    assert_eq!(state.pending_external_cards.len(), 0);
    assert_eq!(
        state.external.len(),
        13,
        "10 inline + 1 restore-paged + 2 drained: one owner each"
    );
    assert_eq!(state.external_card_io_failures, 1);
    assert_eq!(state.external_cards_missing, 0);
}

/// N03: a card grown past the shared blob byte ceiling is refused at the
/// bounded read — typed as corrupt, never read into memory whole; a missing
/// card stays Missing (the pruned-checkpoint case).
#[tokio::test]
async fn an_oversized_card_is_refused_at_the_bounded_read() {
    let dir = tempfile::tempdir().unwrap();
    let cards = dir.path().join("cards");
    std::fs::create_dir_all(&cards).unwrap();
    let id = agent_contracts::ContextItemId::new();
    let card = cards.join(format!("{id}.deadbeefcafe.card"));
    std::fs::write(&card, vec![b'x'; 2 * 1024 * 1024]).unwrap();

    let outcome =
        crate::store::read_external_card_checked_async(&card, id, Some("deadbeefcafe")).await;
    assert!(
        matches!(outcome, crate::store::ExternalCardRead::Corrupt(_)),
        "an oversized card is corrupt, not absent and not an entry"
    );

    let outcome = crate::store::read_external_card_checked_async(
        &cards.join(format!("{}.deadbeefcafe.card", agent_contracts::ContextItemId::new())),
        agent_contracts::ContextItemId::new(),
        Some("deadbeefcafe"),
    )
    .await;
    assert!(matches!(outcome, crate::store::ExternalCardRead::Missing));
}

/// N03 (red-first): the manifest names the card bytes captured at write
/// time. A file at the manifest's name whose bytes hash differently is a
/// different capture wearing the same name — corrupt, never this entry's
/// metadata. The old reader parsed whatever was there and installed it.
#[tokio::test]
async fn a_card_whose_bytes_lost_the_captured_hash_is_corrupt_and_never_installs() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "hash mismatch").await;
    let ids = externalize_n(&engine, 12).await;
    let value = engine.checkpoint().await.unwrap();
    assert_eq!(manifest_ids(&value).len(), 2);
    let target = ids[0];

    // Tamper with the card's bytes under the SAME content-addressed name.
    let hash = engine
        .state
        .lock()
        .await
        .external
        .card_hash(target)
        .unwrap()
        .to_string();
    let card_path = dir.path().join("cards").join(format!("{target}.{hash}.card"));
    let mut card: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&card_path).unwrap()).unwrap();
    card["entry"]["attention"] =
        serde_json::to_value(agent_contracts::AttentionState::Archived).unwrap();
    std::fs::write(&card_path, serde_json::to_vec(&card).unwrap()).unwrap();

    // The store-level read refuses the tampered bytes.
    let outcome =
        crate::store::read_external_card_checked_async(&card_path, target, Some(&hash)).await;
    assert!(
        matches!(outcome, crate::store::ExternalCardRead::Corrupt(_)),
        "bytes that lost the captured hash are corrupt"
    );

    // And hydration never installs them: fresh engine, deferred row.
    let fresh = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 1,
        ..spill_config(&dir, 10)
    });
    fresh.restore(value).await.unwrap();
    fresh.hydrate_pending_cards(2).await;
    let state = fresh.state.lock().await;
    assert!(
        state.external.get(target).is_none(),
        "a tampered card never installs its metadata"
    );
    assert_eq!(
        state.external.recorded_cards(),
        1,
        "only the untouched first card (restore's inline batch) claims itself"
    );
    assert!(
        state.external.card_hash(target).is_none(),
        "nothing claims the tampered card as current metadata"
    );
}

/// N03 (red-first): a deferred card whose entry references a scope this
/// state does not know is the same structural violation
/// `checkpoint::validate` rejects at restore time. The page never installs;
/// the row is consumed and counted as missing (the card file stays on disk
/// for diagnosis).
#[tokio::test]
async fn a_deferred_card_referencing_an_unknown_scope_never_installs() {
    let dir = tempfile::tempdir().unwrap();
    let engine = spill_engine(&dir, 10).await;
    open_focus(&engine, "unknown scope page").await;
    let ids = externalize_n(&engine, 12).await;
    let mut value = engine.checkpoint().await.unwrap();
    // The SECOND spilled id lands in the deferred tail under
    // `external_restore_card_batch: 1` — the page hydration must reject,
    // exactly where restore's own first-batch validation does not reach.
    // (A tampered card inside the first batch is refused by restore's
    // `checkpoint::validate` itself.)
    let target = ids[1];

    // Rewrite the target's card with a scope id no restored scope answers,
    // under the tampered bytes' own content-addressed name.
    let bogus_scope = agent_contracts::ScopeId::new();
    let hash = engine
        .state
        .lock()
        .await
        .external
        .card_hash(target)
        .unwrap()
        .to_string();
    let cards_dir = dir.path().join("cards");
    let old_path = cards_dir.join(format!("{target}.{hash}.card"));
    let mut card: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&old_path).unwrap()).unwrap();
    card["entry"]["scope_id"] = serde_json::to_value(bogus_scope).unwrap();
    let new_bytes = serde_json::to_vec(&card).unwrap();
    let new_hash = crate::store::checksum_hex(&new_bytes)[..12].to_string();
    std::fs::write(
        cards_dir.join(format!("{target}.{new_hash}.card")),
        &new_bytes,
    )
    .unwrap();
    std::fs::remove_file(&old_path).unwrap();
    // The manifest now names the tampered bytes: the hash check passes and
    // the scope validation is what must reject this page.
    let rows = value["external_spilled"].as_array_mut().unwrap();
    let row = rows
        .iter_mut()
        .find(|row| row["id"].as_str() == Some(&target.to_string()))
        .unwrap();
    row["hash"] = serde_json::to_value(&new_hash).unwrap();

    let fresh = SimpleContextEngine::new(SimpleContextConfig {
        external_restore_card_batch: 1,
        ..spill_config(&dir, 10)
    });
    fresh.restore(value).await.unwrap();
    {
        let state = fresh.state.lock().await;
        assert!(
            state
                .pending_external_cards
                .iter()
                .any(|(id, h)| *id == target && *h == new_hash),
            "the deferred row waits under the rewritten card's hash"
        );
    }
    fresh.hydrate_pending_cards(2).await;
    let state = fresh.state.lock().await;
    assert!(
        state.external.get(target).is_none(),
        "an entry referencing an unknown scope never installs"
    );
    assert!(
        state.external_cards_missing >= 1,
        "the invalid page is counted honestly, not silently dropped"
    );
    assert!(
        cards_dir.join(format!("{target}.{new_hash}.card")).exists(),
        "the invalid card file stays on disk: the locator stays diagnosable"
    );
}

//! CTX-9 残余：checkpoint 外置尾分片——最旧的超额 External 条目把元数据
//! 卡片写进既有 store（内容寻址、幂等），checkpoint 只携带内联段＋
//! `external_spilled` 寻址清单；restore 从卡片全量重水化。搜索/召回/
//! 目录语义不变（条目全部驻留内存，分片只压缩 checkpoint 字节）。
//! 卡片缺失/损坏 = 恢复的 external 集合不完整，如实计数、整体不失败。

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextResidency, ContextRetention, ContextScope,
};

use crate::checkpoint;
use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

async fn spill_engine(dir: &tempfile::TempDir, inline_target: usize) -> SimpleContextEngine {
    SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: inline_target,
        external_checkpoint_card_batch: 64,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    })
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

    // 模拟 Storage GC 删除前 10 个分片条目（保留其 blob 的场景按保护根
    // 语义单独验证；这里关心的是卡片清扫跟随条目删除）。
    let removed: std::collections::HashSet<agent_contracts::ContextItemId> =
        spilled[..10].iter().copied().collect();
    {
        let mut state = engine.state.lock().await;
        state
            .external
            .retain(|entry| !removed.contains(&entry.item_id));
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
        io.external_cards_removed, 9,
        "every orphan card goes; only the protected one survives: {:?}",
        io.reasons
    );
    let cards_dir = dir.path().join("cards");
    let card_names: Vec<String> = std::fs::read_dir(&cards_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(card_names.len(), 11, "10 mapped + 1 protected");
    assert!(
        card_names
            .iter()
            .any(|name| name.starts_with(&spilled[0].to_string())),
        "the protected card survives"
    );
}

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

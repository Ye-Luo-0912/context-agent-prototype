//! B2（2026-09-16 review 3bdb269c 后 NEXT_TASKS 遗留）：首次认领 existing
//! card 的校验与取消安全（取消安全的 B3 部分已由 `3fe396c9` 收口）。
//!
//! `run_external_spill_io` 的 plan.writes 分支对已存在的卡片路径只做
//! `try_exists` 就直接放进 `io.written/io.spilled`：既不比对现有内容与
//! 计划字节，也不走受检卡片读取。卡片路径是内容寻址的
//! （`cards/<id>.<hash>.card`，哈希由计划字节导出）——同名异字节只能是
//! 坏卡/异物（崩溃残片、截断、同名覆盖）。认领它之后 checkpoint 会
//! `record_card` 并把它从 inline 段排除：新 checkpoint 只引用坏卡片，
//! 下次 restore 才发现（原元数据失去唯一可靠副本）。
//!
//! 反例：无有效 claim 的 fixture 中预置同名坏文件/同名目录 → capture →
//! 新引擎 restore。坏字节：capture 必须以计划字节修复认领，restore 后
//! 元数据可恢复。目录占位：无法修复时 capture 明确保留 inline，restore
//! 直接拿到元数据，不留下指向占位物的定位行。

use agent_contracts::{
    ContextEngine, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};
use crate::store::{checksum_hex, external_card_bytes, external_card_path, store_dir};

use super::harness::open_focus;

fn b2_config(dir: &tempfile::TempDir) -> SimpleContextConfig {
    SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        // restore 一张不装：所有条目以 pending 定位行留给按 id 读取，
        // 证明证据走的是卡片 manifest 而不是 inline 段（目录占位反例里
        // 再断言它确实留在 inline）。
        external_restore_card_batch: 0,
        gc_buffer_capacity: 0,
        external_checkpoint_io_budget_ms: 60_000,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

/// Externalize one note (empty eviction buffer → straight to the store) and
/// pin its residency at External so the spill gate sees it. Returns the id.
async fn externalize_one(engine: &SimpleContextEngine, body: &str) -> ContextItemId {
    let id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            body.into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("b2".into()),
        );
        item.scope_id = None;
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        let id = item.id;
        state.eviction_buffer.push(item);
        id
    };
    engine.gc().await.unwrap();
    {
        let mut state = engine.state.lock().await;
        let entry = state.external.get_mut(id).expect("externalized");
        entry.residency = ContextResidency::External;
    }
    id
}

/// The card path this capture will claim for `id`：序列化与
/// `plan_external_spill` 同源（同一 entry、同一 serializer、同一哈希截断）。
async fn planned_card_path(engine: &SimpleContextEngine, id: ContextItemId) -> std::path::PathBuf {
    let entry = {
        let state = engine.state.lock().await;
        state.external.get(id).expect("entry resident").clone()
    };
    let bytes = external_card_bytes(&entry);
    let hash = checksum_hex(&bytes)[..12].to_string();
    external_card_path(&store_dir(&engine.config), id, &hash)
}

async fn pending_row_for(engine: &SimpleContextEngine, id: ContextItemId) -> bool {
    let state = engine.state.lock().await;
    state
        .pending_external_cards
        .iter()
        .any(|(row_id, _)| *row_id == id)
}

/// B2 核心反例（红-first）：同名坏文件存在且无有效 claim → capture 不得
/// 把它当有效卡片认领。修复路径：读回不一致 → 以计划字节原子重写 →
/// manifest 引用的是好字节 → restore 后元数据可恢复。旧代码直接认领坏
/// 文件，restore 消费坏卡（Corrupt），元数据丢失。
#[tokio::test]
async fn capture_repairs_a_same_name_card_that_does_not_match_the_planned_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimpleContextEngine::new(b2_config(&dir));
    open_focus(&source, "claim integrity probe").await;
    let target = externalize_one(&source, "payload-claim-integrity").await;

    // 在 capture 之前预置同名坏字节：无任何有效 claim。
    let poison = planned_card_path(&source, target).await;
    tokio::fs::create_dir_all(poison.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&poison, b"foreign garbage bytes, definitely not a card")
        .await
        .unwrap();

    let checkpoint = source.checkpoint().await.unwrap();

    // 认领（修复）之后，路径上是计划字节本身。
    let entry = {
        let state = source.state.lock().await;
        state.external.get(target).expect("entry resident").clone()
    };
    let reread = tokio::fs::read(&poison).await.unwrap();
    assert_eq!(
        reread,
        external_card_bytes(&entry),
        "the claimed card must hold the planned bytes, not the pre-placed garbage"
    );

    // 新引擎恢复：定位行在 manifest 里（restore 批次为 0，全 defer），
    // 按 id 读取必须拿到原元数据——坏引用不再存在。
    let restored = SimpleContextEngine::new(b2_config(&dir));
    restored.restore(checkpoint).await.unwrap();
    assert!(
        pending_row_for(&restored, target).await,
        "setup: the card row stays deferred for the by-id read"
    );
    let fetched = restored
        .fetch_external(target)
        .await
        .unwrap()
        .expect("the repaired card must restore the metadata by id");
    assert!(
        fetched.content.contains("payload-claim-integrity"),
        "the fetched body is the captured one: {}",
        fetched.content
    );
}

/// 目录占位：`try_exists` 对目录同样为真——旧代码把目录当有效卡片认领，
/// checkpoint 留下指向占位物的定位行，restore 按 id 读取失败（读取失败
/// 行被保留，元数据却已经不在 inline 段）。修复：无法修复成计划字节时
/// 本次保持 inline，元数据直接随 checkpoint 可恢复。
#[tokio::test]
async fn capture_keeps_an_entry_inline_when_its_card_path_is_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimpleContextEngine::new(b2_config(&dir));
    open_focus(&source, "directory claim probe").await;
    let target = externalize_one(&source, "payload-directory-claim").await;

    let poison = planned_card_path(&source, target).await;
    tokio::fs::create_dir_all(&poison).await.unwrap();

    let checkpoint = source.checkpoint().await.unwrap();

    let restored = SimpleContextEngine::new(b2_config(&dir));
    restored.restore(checkpoint).await.unwrap();
    assert!(
        !pending_row_for(&restored, target).await,
        "an unclaimable card path must keep the entry inline — no manifest row may point at it"
    );
    {
        let state = restored.state.lock().await;
        assert!(
            state.external.get(target).is_some(),
            "the entry's metadata is recoverable straight from the checkpoint"
        );
    }
    let fetched = restored
        .fetch_external(target)
        .await
        .unwrap()
        .expect("inline metadata must fetch by id");
    assert!(
        fetched.content.contains("payload-directory-claim"),
        "the fetched body is the captured one: {}",
        fetched.content
    );
}

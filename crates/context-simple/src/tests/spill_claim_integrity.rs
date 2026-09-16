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
use crate::store::{
    checksum_hex, external_card_bytes, external_card_path, read_existing_card_bounded, store_dir,
};

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

/// O2（2026-09-16 review 980bbc77 遗留观察）：认领校验的读取必须有并发
/// 硬界——前置 metadata（无论 pathname 还是句柄级）不是实际 read 的硬
/// 上限，并发替换/增长可以让「长度检查通过」之后真正读到的内容任意大。
/// 读取量必须由落在句柄上的 `take(计划长度+1)` 结构性钉死。
///
/// 竞态本身（metadata 与 read 之间文件增长）无法确定性构造；这里直接
/// 断言它的结构后置条件：文件比计划字节长 16 MiB 时，校验读取拉回的
/// 字节数**恰好**是 expected_len+1——helper 返回读到的字节，读取量直接
/// 可断言，无界整读（变异）会读到全部后缀，这条断言即红。等长文件仍
/// 精确相等，证明有界化没有破坏认领的匹配面。
#[tokio::test]
async fn claim_check_read_is_capped_at_the_planned_length_plus_one_byte() {
    let dir = tempfile::tempdir().unwrap();
    let expected = b"planned card bytes".to_vec();

    // 巨大后缀：计划字节之后接 16 MiB 异物。有界读取一次也不把它拉进来。
    let mut oversized = expected.clone();
    oversized.extend(vec![b'x'; 16 * 1024 * 1024]);
    let long_path = dir.path().join("oversized.card");
    tokio::fs::write(&long_path, &oversized).await.unwrap();

    let got = read_existing_card_bounded(&long_path, expected.len())
        .await
        .expect("a plain file must be readable");
    assert_eq!(
        got.len(),
        expected.len() + 1,
        "the claim check must read at most planned length + 1 bytes, \
         never the whole oversized file"
    );
    assert_ne!(got, expected, "a longer file can never match the plan");

    // 等长文件：读到 planned_len（上限之内），精确相等 → 可认领。
    let exact_path = dir.path().join("exact.card");
    tokio::fs::write(&exact_path, &expected).await.unwrap();
    let got = read_existing_card_bounded(&exact_path, expected.len())
        .await
        .expect("a plain file must be readable");
    assert_eq!(got, expected, "an equal-length file must still match");
}

/// O2 行为面：比计划字节长的同名文件不可认领——与坏字节同一 fail-closed
/// 语义（读回不一致 → 以计划字节原子重写修复；这里预置计划字节＋巨大
/// 后缀，修复后路径上必须只剩计划字节，restore 按 id 拿回原元数据）。
/// 旧代码的 metadata 前置长度检查对该静态文件同样拒绝，故本条是行为
/// 回归守卫；「读取量有界」由上一条的 helper 层断言直接承证。
#[tokio::test]
async fn capture_repairs_a_card_that_is_longer_than_the_planned_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimpleContextEngine::new(b2_config(&dir));
    open_focus(&source, "oversized claim probe").await;
    let target = externalize_one(&source, "payload-oversized-claim").await;

    // 认领路径上预置计划字节＋巨大后缀：无任何有效 claim。
    let poison = planned_card_path(&source, target).await;
    tokio::fs::create_dir_all(poison.parent().unwrap())
        .await
        .unwrap();
    let mut oversized = {
        let state = source.state.lock().await;
        external_card_bytes(state.external.get(target).expect("entry resident"))
    };
    oversized.extend(vec![b'S'; 8 * 1024 * 1024]);
    tokio::fs::write(&poison, &oversized).await.unwrap();

    let checkpoint = source.checkpoint().await.unwrap();

    // 修复之后，路径上是计划字节本身——超长异物被整个替换掉。
    let entry = {
        let state = source.state.lock().await;
        state.external.get(target).expect("entry resident").clone()
    };
    let reread = tokio::fs::read(&poison).await.unwrap();
    assert_eq!(
        reread,
        external_card_bytes(&entry),
        "the oversized candidate must be repaired to exactly the planned bytes"
    );

    // 新引擎恢复：修复后的卡片按 id 读回原元数据。
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
        fetched.content.contains("payload-oversized-claim"),
        "the fetched body is the captured one: {}",
        fetched.content
    );
}

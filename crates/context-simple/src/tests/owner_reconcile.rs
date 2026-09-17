//! G1（2026-09-18 审查）：逻辑 owner 驱动的 reconcile。
//!
//! 固定小热预算下，冷历史以 pending 行持有（卡片与 blob 都在盘上，元数据
//! 未装载）。reconcile 的 hydration 因 HotCap 停止后，旧扫描只看已装载
//! 视图，把 pending 持有的 blob 当无主孤儿重新认领进热表：同一逻辑 ID
//! 双重所有权、热预算失守，blob 重建元数据还会顶掉冷卡片捕获的版本。
//! 本模块钉住四条规则：
//!
//! - pending owner ≠ ownerless：未读冷卡片意味着「详情未知」，不是「无主」；
//! - commit 在锁下复核所有 owner 位置，只接纳真正的孤儿；
//! - 合法孤儿接纳后走既有 `settle_metadata_residency` 预算结算（可观测的
//!   让位/背压，不静默撑大热表）；
//! - 重复 reconcile 幂等；checkpoint/restore 往返后冷正文仍可按 id 取回。
//!
//! 附带正对照：恢复功能没有被一刀切关闭——真正无主的 blob 仍被认领。

use agent_contracts::{
    ContextEngine, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
};

use crate::engine::{HydrationBudget, HydrationStop, SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 统一的预算构造：条数/字节上限显式给定，deadline 远期（本模块不钉
/// 时钟）。
fn budget(max_items: usize, hot_max_entries: usize, hot_max_bytes: u64) -> HydrationBudget {
    HydrationBudget {
        max_items,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        hot_max_entries,
        hot_max_bytes,
    }
}

/// 一段全部卡片化、且卡片版本与 blob 版本可区分的历史：n 条笔记经真实
/// spill 路径外置（blob 冻结外置时的 importance 快照 0.5），随后把内存
/// entry 的 importance 全部改写为 0.9 再 checkpoint——卡片因此携带 0.9，
/// blob 仍是 0.5。restore 侧任何按卡片版本的取回应看到 0.9；被 blob
/// 重建条目顶替则回落到 0.5。
async fn divergent_carded_history(
    dir: &tempfile::TempDir,
    n: usize,
) -> (serde_json::Value, Vec<ContextItemId>) {
    let first = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        // capture 的卡片写入有墙钟预算（慢 CI 上多次小文件写入可越过默认
        // 2s），这里钉宽——同 fixed_budget_closure 的处理。
        external_checkpoint_io_budget_ms: 60_000,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&first, "owner driven reconcile").await;
    let mut ids = Vec::new();
    for index in 0..n {
        {
            let mut state = first.state.lock().await;
            let mut body = format!("owner body {index} unique-token-{index} ");
            while body.chars().count() < 121 {
                body.push('p');
            }
            let mut item = crate::item::make_item(
                &state,
                &first.config,
                body,
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                Some("owner-reconcile".into()),
            );
            item.residency = ContextResidency::Warm;
            item.evicted_at_tick = Some(0);
            ids.push(item.id);
            state.eviction_buffer.push(item);
        }
        first.gc().await.unwrap();
    }
    {
        let mut state = first.state.lock().await;
        for id in &ids {
            let entry = state.external.get_mut(*id).expect("externalized");
            entry.residency = ContextResidency::External;
            entry.importance = 0.9; // 卡片版本与 blob 快照分叉
        }
    }
    let value = first.checkpoint().await.unwrap();
    let spilled = value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .expect("inline target 0 spills the whole tail")
        .len();
    assert_eq!(spilled, n, "every cold entry is a manifest row");
    (value, ids)
}

/// 把历史 restore 进一个热上限 `hot_cap`、restore 批 1 的引擎：最旧一条
/// 装载为热条目（卡片 claim 已登记），其余成为 pending 冷行——正是有界
/// restore 返回时的真实形状。
async fn capped_engine(
    dir: &tempfile::TempDir,
    value: &serde_json::Value,
    hot_cap: usize,
) -> SimpleContextEngine {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 1,
        external_hot_metadata_max_entries: hot_cap,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value.clone()).await.unwrap();
    engine
}

/// 当前 owner 拓扑的快照断言集：hot 与 pending 互斥、并集恰为全历史、热
/// 表不越上限。返回 pending 行（含卡片哈希）供身份比较。
async fn assert_single_ownership(
    engine: &SimpleContextEngine,
    expected: &[ContextItemId],
    hot_cap: usize,
    context: &str,
) -> Vec<(ContextItemId, String)> {
    let state = engine.state.lock().await;
    let hot: std::collections::HashSet<_> = state.external.iter().map(|e| e.item_id).collect();
    let rows = state.pending_external_cards.clone();
    let pending: std::collections::HashSet<_> = rows.iter().map(|(id, _)| *id).collect();
    assert!(
        hot.is_disjoint(&pending),
        "{context}: no logical id may be owned twice (hot {hot:?} vs pending {pending:?})"
    );
    let mut owners = hot.clone();
    owners.extend(&pending);
    let expected: std::collections::HashSet<_> = expected.iter().copied().collect();
    assert_eq!(owners, expected, "{context}: the owner set is conserved");
    assert!(
        state.external.len() <= hot_cap,
        "{context}: the hot budget holds ({} > {hot_cap})",
        state.external.len()
    );
    rows
}

/// G1 反例（红-first）：热上限 1，热 A，pending B/C（blob 与卡片齐备）。
/// hydration 因 HotCap 停止后，旧形状的扫描把 B/C 当无主孤儿重新认领进
/// 热表——hot 1→3、pending 仍在、B/C 同时在两处。修复后：每个逻辑 ID
/// 恰一份 owner，各驻留集合互斥，热预算不失守，pending 行身份（卡片
/// 哈希）不被 blob 状态覆盖，按 id 取回由卡片版本应答。
#[tokio::test]
async fn reconcile_does_not_reclaim_a_pending_cold_owners_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (value, ids) = divergent_carded_history(&dir, 3).await;
    let engine = capped_engine(&dir, &value, 1).await;

    // 前置：热 1（A），pending 2（B/C），blob 与卡片都在盘上。
    let pending_rows = {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            1,
            "the capped restore pages one entry in"
        );
        assert_eq!(state.pending_external_cards.len(), 2);
        state.pending_external_cards.clone()
    };
    for (id, _) in &pending_rows {
        assert!(
            dir.path().join(format!("{id}.json")).exists(),
            "the pending owner's blob is on disk"
        );
    }

    // 证明本次 hydration 确实因 HotCap 停止：目标 ID 这轮没有被装载。
    let outcome = engine
        .hydrate_within_budget(budget(100, 1, u64::MAX), &[])
        .await;
    assert!(!outcome.complete, "{outcome:?}");
    assert_eq!(outcome.stopped, HydrationStop::HotCap, "{outcome:?}");
    assert_eq!(outcome.remaining, 2, "B/C stay unread this round");
    {
        let state = engine.state.lock().await;
        for (id, _) in &pending_rows {
            assert!(state.external.get(*id).is_none(), "not loaded this round");
        }
    }

    let report = engine.reconcile_store_protecting(&[], true).await.unwrap();

    let rows_after = assert_single_ownership(&engine, &ids, 1, "after the reconcile").await;
    assert_eq!(
        rows_after, pending_rows,
        "the pending card identity (id, hash) is not overwritten by blob-derived state"
    );
    assert_eq!(
        report.rebuilt, 0,
        "a pending owner is not re-adopted: {report:?}"
    );
    assert_eq!(
        report.deleted_stale, 0,
        "owned blobs are not deleted: {report:?}"
    );
    assert_eq!(
        report.external_cards_removed, 0,
        "owned cards are not swept: {report:?}"
    );

    // 按卡片版本应答：取回 pending 正文时 importance 是卡片捕获的 0.9，
    // 不是 blob 快照的 0.5。
    let b_id = pending_rows[0].0;
    let fetched = engine
        .fetch_external(b_id)
        .await
        .unwrap()
        .expect("the pending body still fetches");
    assert!(
        fetched.content.contains("unique-token"),
        "the captured body serves: {}",
        fetched.content
    );
    assert_eq!(
        fetched.importance, 0.9,
        "the card version answers, not the blob-derived snapshot"
    );
}

/// G1 正对照：恢复功能没有被一刀切关掉——一个任何位置都无 owner 的合法
/// blob 仍被认领（typed `rebuilt`），并且接纳后走既有的热驻留结算：挤出
/// 固定热上限的部分由最旧的已卡片化条目让位回 pending（可观测），而不是
/// 静默把热表撑大。被让位条目的 blob 仍在盘上，两条正文都仍可按 id 取回。
#[tokio::test]
async fn a_true_orphan_is_adopted_through_the_residency_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let (value, ids) = divergent_carded_history(&dir, 3).await;
    let engine = capped_engine(&dir, &value, 1).await;
    let hot_before = {
        let state = engine.state.lock().await;
        assert_eq!(state.external.len(), 1, "setup: one hot entry A");
        state.external.iter().next().map(|e| e.item_id).unwrap()
    };

    // 一个真正的孤儿：合法 blob，无卡片，任何 owner 位置都不持有。
    let orphan_id = {
        let state = engine.state.lock().await;
        let item = crate::item::make_item(
            &state,
            &engine.config,
            "true orphan body orphan-token".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Working,
            0.5,
            Some("owner-reconcile".into()),
        );
        let id = item.id;
        crate::store::externalize(dir.path(), &item).unwrap();
        id
    };

    let report = engine.reconcile_store_protecting(&[], true).await.unwrap();
    assert_eq!(
        report.rebuilt, 1,
        "the true orphan is adopted, not silently ignored: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(orphan_id).is_some(),
            "the adopted entry owns the orphan's blob"
        );
        assert!(
            state.external.len() <= 1,
            "the settlement keeps the hot cap: {}",
            state.external.len()
        );
        assert!(
            state
                .pending_external_cards
                .iter()
                .any(|(id, _)| *id == hot_before),
            "the settlement demoted the oldest carded entry back to pending — \
             adoption went through the existing residency path"
        );
        assert!(
            dir.path().join(format!("{hot_before}.json")).exists(),
            "the demoted owner's blob stays on disk"
        );
    }

    // 认领后的正文与被让位条目的正文都仍可取回。
    let fetched = engine
        .fetch_external(orphan_id)
        .await
        .unwrap()
        .expect("the adopted orphan's body serves");
    assert!(
        fetched.content.contains("orphan-token"),
        "{}",
        fetched.content
    );
    let demoted = engine
        .fetch_external(hot_before)
        .await
        .unwrap()
        .expect("the demoted entry's body still serves");
    assert!(
        demoted.content.contains("unique-token"),
        "{}",
        demoted.content
    );
    let _ = ids;
}

/// G1 回归：固定热预算下重复 reconcile 幂等——owner 集合不变、各层互斥、
/// pending 行身份保持，第二次 pass 不再发生任何认领、删除或清扫。
#[tokio::test]
async fn repeated_reconcile_over_a_fixed_hot_budget_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let (value, ids) = divergent_carded_history(&dir, 3).await;
    let engine = capped_engine(&dir, &value, 1).await;

    let rows_first = assert_single_ownership(&engine, &ids, 1, "setup").await;
    engine.reconcile_store_protecting(&[], true).await.unwrap();
    let rows_second = assert_single_ownership(&engine, &ids, 1, "after the first reconcile").await;
    assert_eq!(rows_second, rows_first, "cold card versions are preserved");

    let report = engine.reconcile_store_protecting(&[], true).await.unwrap();
    let rows_third = assert_single_ownership(&engine, &ids, 1, "after the second reconcile").await;
    assert_eq!(rows_third, rows_first, "cold card versions are preserved");
    assert_eq!(report.rebuilt, 0, "no second-wave adoption: {report:?}");
    assert_eq!(report.deleted_stale, 0, "{report:?}");
    assert_eq!(report.quarantined, 0, "{report:?}");
    assert_eq!(report.external_cards_removed, 0, "{report:?}");
}

/// G1 回归：counterexample 场景下 checkpoint/restore 往返后，冷历史仍可
/// 按 id 取回，且取回的是卡片捕获的版本。
#[tokio::test]
async fn checkpoint_restore_roundtrip_keeps_cold_bodies_fetchable() {
    let dir = tempfile::tempdir().unwrap();
    let (value, ids) = divergent_carded_history(&dir, 3).await;
    let engine = capped_engine(&dir, &value, 1).await;
    engine.reconcile_store_protecting(&[], true).await.unwrap();
    assert_single_ownership(&engine, &ids, 1, "after the reconcile").await;

    let snapshot = engine.checkpoint().await.unwrap();
    let second = capped_engine(&dir, &snapshot, 1).await;
    assert_single_ownership(&second, &ids, 1, "after the restore").await;

    for (index, id) in ids.iter().enumerate() {
        let fetched = second
            .fetch_external(*id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("fetch {index} must serve the body after the roundtrip"));
        assert!(
            fetched.content.contains(&format!("unique-token-{index}")),
            "fetch {index} returns the captured body: {}",
            fetched.content
        );
        assert_eq!(
            fetched.importance, 0.9,
            "fetch {index} serves the card-captured version"
        );
    }

    // 往返后再 reconcile，不变量仍然成立。
    second.reconcile_store_protecting(&[], true).await.unwrap();
    assert_single_ownership(&second, &ids, 1, "after the post-roundtrip reconcile").await;
}

//! T4：真正有界的冷目录、搜索与维护工作（第一期）——每操作预算＋类型化不完整。
//!
//! 审查基线（2026-09-14 review T4 节）：「每批有限，不代表一次操作总工作量
//! 有限；checkpoint 文件变小，也不代表内存占用不再跟随历史增长。」旧形状的
//! `hydrate_all_pending_cards` 循环到队列为空为止：一次 search/GC/reconcile
//! 就把整段历史的冷元数据全部装进热目录——总量跟随历史长度。
//!
//! 本模块钉住五条规则：
//! - 每操作预算（items ＋ 绝对 deadline ＋ 热元数据上限）内返回；
//! - 预算耗尽不抛错：类型化报告剩余量（数量＋停止原因），pending 队列不动；
//! - 热上限超限不再无界重水化——按 id 分页服务继续可用，owner 一个不丢；
//! - 未读区域可继续：后续操作能继续取回（每 id 或提额后的成批续排空）；
//! - 搜索语义保持 B2：未读区域不得报成完整零命中。

use agent_contracts::{
    ContextEngine, ContextKind, ContextResidency, ContextRetention, ContextScope,
    ContextSearchQuery,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 冷集合规模 = items 预算 × 5：明显大于热预算的声明规模。
const COLD_TOTAL: usize = 40;
const HYDRATE_ITEMS_BUDGET: usize = 8;
const HOT_METADATA_CAP: usize = 16;
const RESTORE_BATCH: usize = 8;

fn cold_bounds_config(dir: &tempfile::TempDir) -> SimpleContextConfig {
    SimpleContextConfig {
        // 全部外置尾都进卡片（inline 目标 0）：checkpoint 只携带寻址清单。
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: RESTORE_BATCH,
        external_hydrate_max_items: HYDRATE_ITEMS_BUDGET,
        external_hot_metadata_max_entries: HOT_METADATA_CAP,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

/// Externalize `n` notes (empty eviction buffer → straight to the store) and
/// pin their residency at External so the spill gate sees them. Bodies carry
/// a per-index token so search can hit them, in externalization order.
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
                Some("cold-bounds".into()),
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

/// Build a capture of `COLD_TOTAL` cold entries (all spilled to cards, no
/// inline tail), and return it with the capturing engine (whose hot map
/// still holds the full history, for the byte-footprint comparison).
async fn captured_history(
    dir: &tempfile::TempDir,
) -> (
    SimpleContextEngine,
    serde_json::Value,
    Vec<agent_contracts::ContextItemId>,
) {
    let first = SimpleContextEngine::new(cold_bounds_config(dir));
    open_focus(&first, "cold bounds").await;
    let ids = externalize_n(&first, COLD_TOTAL).await;
    let value = first.checkpoint().await.unwrap();
    let spilled = value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .expect("inline target 0 spills the whole tail")
        .len();
    assert_eq!(spilled, COLD_TOTAL, "every cold entry is a manifest row");
    (first, value, ids)
}

/// T4 (red-first): 一次 search/GC 往返后，热资源不得跟随全历史增长。
///
/// 旧形状在第一次 search 里把 32 条 pending 卡片全部重水化：热目录达到 40
/// 条（全历史），后续 search 全量通过、GC 完整无延期。新形状：每操作最多
/// 预算内取卡，热目录停在配置上限，超出部分类型化报告剩余量并可继续。
#[tokio::test]
async fn a_cold_collection_larger_than_the_hot_budget_stays_bounded_and_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let (first, value, ids) = captured_history(&dir).await;
    let full_history_bytes = first.state.lock().await.external.serialized_bytes();
    let full_history_entries = first.state.lock().await.external.len();
    assert_eq!(full_history_entries, COLD_TOTAL);

    // 冷恢复：restore 只读一个批次，其余是 pending 行——id 可知、元数据
    // 不在内存。
    let mut engine = SimpleContextEngine::new(cold_bounds_config(&dir));
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            RESTORE_BATCH,
            "restore pages exactly one batch in"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            COLD_TOTAL - RESTORE_BATCH,
            "the rest stay a resumable pending directory"
        );
    }

    // 按 id fetch：per-id 服务一次一卡，与预算/上限无关。
    let pending_first = engine.state.lock().await.pending_external_cards[0].0;
    let fetched = engine.fetch_external(pending_first).await.unwrap().unwrap();
    assert!(
        fetched.content.contains("unique-token"),
        "the paged-in body is the captured one: {}",
        fetched.content
    );

    // 搜索：非空命中照常返回（ranked Top-K 从不声称完整）；同一次操作的
    // 重水化被预算与热上限夹住——热目录恰好到上限，而不是全历史。
    let hits = engine
        .search_external(ContextSearchQuery::new("unique-token", 8))
        .await
        .expect("non-empty hits are returned as-is");
    assert!(!hits.is_empty(), "the hot region matches the query");
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            HOT_METADATA_CAP,
            "the search's drain fills the hot directory exactly to the cap, not to history"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            COLD_TOTAL - HOT_METADATA_CAP,
            "the unread remainder stays queued (resumable, owner intact)"
        );
    }

    // 热上限处：空命中查询必须 fail-closed（B2），错误携带类型化剩余量，
    // 且这次操作不再增长热资源。旧形状在此全量排空后返回 Ok(空)——完整
    // 零命中的假象。
    let outcome = engine
        .search_external(ContextSearchQuery::new("zzz-no-such-token", 8))
        .await;
    let error = outcome.expect_err("an empty result over an unread region must fail closed");
    let error = error.to_string();
    assert!(
        error.contains("coverage incomplete"),
        "the typed coverage error names the state: {error}"
    );
    assert!(
        error.contains("24"),
        "the error carries the exact unread remainder: {error}"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            HOT_METADATA_CAP,
            "a cap-stopped search does not grow the hot directory"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            COLD_TOTAL - HOT_METADATA_CAP,
            "the pending queue is untouched by the capped pass"
        );
    }

    // Storage GC：元数据不完整 → 删除延期（既有 B2 模式消费类型化结果），
    // 报告命名可继续的剩余量。旧形状完整排空后不延期。
    let report = engine.storage_gc_protecting(&[], true).await.unwrap();
    assert_eq!(report.deleted, 0, "an incomplete pass deletes nothing");
    assert!(
        report
            .reasons
            .iter()
            .any(|row| row.contains("deletion deferred") && row.contains("24 pending spill row(s)")),
        "the deferral names the resumable remainder: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_external_cards.len(),
            COLD_TOTAL - HOT_METADATA_CAP,
            "the deferred pass leaves the queue resumable"
        );
        assert!(
            state.external.get(pending_first).is_some(),
            "no owner was dropped to enforce the hot cap"
        );
    }

    // 未读部分可继续（每 id 服务）：热上限处，pending 行仍能按 id 取回。
    let pending_second = engine.state.lock().await.pending_external_cards[0].0;
    assert_ne!(pending_second, pending_first);
    let fetched = engine
        .fetch_external(pending_second)
        .await
        .unwrap()
        .unwrap();
    assert!(fetched.content.contains("unique-token"));

    // 续排空：提高热上限后，后续 search 继续消费预算内的 pending 行，直到
    // 队列真正排空——此时完整零命中才是合法结果。
    engine.config.external_hot_metadata_max_entries = COLD_TOTAL * 2;
    let mut passes = 0;
    loop {
        passes += 1;
        let state = engine.state.lock().await;
        let pending_left = state.pending_external_cards.len();
        drop(state);
        if pending_left == 0 {
            break;
        }
        assert!(
            passes <= 8,
            "each pass consumes at most the items budget; the queue must converge"
        );
        engine
            .search_external(ContextSearchQuery::new("unique-token", 8))
            .await
            .unwrap();
    }
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            COLD_TOTAL,
            "the resumed drains eventually page the whole collection back in"
        );
    }
    let empty = engine
        .search_external(ContextSearchQuery::new("zzz-no-such-token", 8))
        .await
        .unwrap();
    assert!(
        empty.is_empty(),
        "with hydration complete, a zero-match may finally be reported as complete"
    );
    let _ = ids;

    // 热资源不随全历史永久增长：在预算内往返后（提额续排空之前的形状），
    // 热目录规模与字节都远小于全历史。这里按第一步的口径复验：新建一个
    // 同配置引擎重放「恢复→fetch→search」往返，热资源停在预算量级。
    let (_, value2, _) = captured_history(&dir).await;
    let bounded = SimpleContextEngine::new(cold_bounds_config(&dir));
    bounded.restore(value2).await.unwrap();
    let id0 = bounded.state.lock().await.pending_external_cards[0].0;
    bounded.fetch_external(id0).await.unwrap().unwrap();
    bounded
        .search_external(ContextSearchQuery::new("unique-token", 8))
        .await
        .unwrap();
    {
        let state = bounded.state.lock().await;
        let hot_entries = state.external.len();
        let hot_bytes = state.external.serialized_bytes();
        assert!(
            hot_entries <= HOT_METADATA_CAP + 1,
            "after a restore->fetch->search round trip the hot directory is bounded by the \
             budget (+ the one explicitly fetched id), not by history: {hot_entries}"
        );
        assert!(
            hot_entries * 2 < full_history_entries,
            "hot residency is not proportional to history: {hot_entries} of {full_history_entries}"
        );
        assert!(
            hot_bytes * 2 < full_history_bytes,
            "resident metadata bytes do not track the full history: {hot_bytes} of \
             {full_history_bytes}"
        );
    }
}

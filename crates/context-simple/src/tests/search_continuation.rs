//! W2（2026-09-16 review V4/V5，红-first）：搜索 continuation 的请求级边界
//! 与生命周期。
//!
//! V4：`record_search_coverage` 曾在 query_key 相同时无去重追加旧
//! covered_ids——固定历史下重复普通（无 token）搜索即可让 retained state
//! 随调用次数线性增长。修复后 fresh（开始新遍历、不继承）与 resume（验证
//! 过的遍历、去重、链级上限）分开。
//!
//! V5：`restore` 曾只替换 State 不清理 State 之外的续查状态；token 编号由
//! slot 尾数推导、链完成清空重开——旧 token 可在原位 restore 后或新链 ABA
//! 命中。修复后成功 restore 使旧 token 失效；token 是进程生命周期单调
//! nonce 并绑定恢复代际；被拒 token 明确按 fresh 遍历处理，不混入不同
//! 遍历状态。

use std::collections::HashSet;

use agent_contracts::{
    ContextEngine, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
    ContextSearchCoverageStop, ContextSearchQuery,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 与 fixed_budget_closure 相同的固定冷热夹具形状：`n` 条等长正文全部卡片
/// 化（inline 目标 0），restore 批次与热上限都钉在 `cap`，所以第一次搜索后
/// 恰好剩 `n - cap` 张未读冷页。
fn tight_config(dir: &tempfile::TempDir, cap: usize) -> SimpleContextConfig {
    SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: cap,
        external_hot_metadata_max_entries: cap,
        gc_buffer_capacity: 0,
        external_checkpoint_io_budget_ms: 60_000,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

/// 建立一段全部卡片化的历史，返回 (checkpoint, ids)。
async fn carded_history(
    dir: &tempfile::TempDir,
    n: usize,
) -> (serde_json::Value, Vec<ContextItemId>) {
    let writer = SimpleContextEngine::new(tight_config(dir, n));
    open_focus(&writer, "search continuation lifecycle").await;
    let mut ids = Vec::new();
    for index in 0..n {
        {
            let mut state = writer.state.lock().await;
            let mut body = format!("spill body {index} unique-token-{index} ");
            while body.chars().count() < 121 {
                body.push('p');
            }
            let mut item = crate::item::make_item(
                &state,
                &writer.config,
                body,
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                Some("search-continuation".into()),
            );
            item.residency = ContextResidency::Warm;
            item.evicted_at_tick = Some(0);
            ids.push(item.id);
            state.eviction_buffer.push(item);
        }
        writer.gc().await.unwrap();
    }
    {
        let mut state = writer.state.lock().await;
        for id in &ids {
            state.external.get_mut(*id).expect("externalized").residency =
                ContextResidency::External;
        }
    }
    let value = writer.checkpoint().await.unwrap();
    (value, ids)
}

/// 当前热目录里有卡片 claim 的 id 集合（断言「覆盖集合 == 本次遍历自己看
/// 过的窗口」用）。
async fn carded_hot_ids(engine: &SimpleContextEngine) -> HashSet<ContextItemId> {
    engine
        .state
        .lock()
        .await
        .external
        .carded_hot_ids()
        .into_iter()
        .collect()
}

const CAP: usize = 4;
const N: usize = 14;

/// V4 反例（红-first）：固定 hot/pending 集合，连续重复普通（无 token）
/// 查询 N 次——续查状态内存不得随 N 增长。旧代码每个 pass 无去重追加同一
/// 批热窗口 ID，covered_ids 线性膨胀（4、8、12…）。
#[tokio::test]
async fn repeated_plain_searches_do_not_grow_the_continuation_state() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value).await.unwrap();

    let mut first_footprint: Option<(String, Vec<ContextItemId>)> = None;
    for round in 0..12 {
        let hits = engine
            .search_external(ContextSearchQuery::new("unique-token", 20))
            .await
            .expect("non-empty partial hits are returned, not dropped");
        assert!(
            !hits.is_empty(),
            "round {round}: the hot window still matches"
        );
        let coverage = engine.last_search_coverage();
        assert!(
            !coverage.complete && coverage.unread_pages == N - CAP,
            "round {round}: repeated plain searches see the same fixed fixture: {coverage:?}"
        );
        let (token, covered) = engine
            .search_continuation_probe()
            .expect("an incomplete pass keeps a live walk");
        match &first_footprint {
            None => {
                // 第一个 pass 只覆盖本次遍历自己看到的窗口，不继承任何旧集合。
                assert!(
                    covered.len() <= CAP,
                    "round {round}: a fresh walk covers at most the hot window ({} ids): {covered:?}",
                    covered.len()
                );
                let hot = carded_hot_ids(&engine).await;
                let covered_set: HashSet<_> = covered.iter().copied().collect();
                assert_eq!(
                    covered_set, hot,
                    "round {round}: the covered set is exactly this pass's window"
                );
                first_footprint = Some((token, covered));
            }
            Some((first_token, first_covered)) => {
                // 内存断言用原始条数（不去重）：旧代码每个 pass 无去重追加同一
                // 批热窗口 ID，retained state 随调用次数线性膨胀。
                assert_eq!(
                    covered.len(),
                    first_covered.len(),
                    "round {round}: repeated plain searches must not grow the retained \
                     continuation state ({} ids vs first {})",
                    covered.len(),
                    first_covered.len()
                );
                let covered_set: HashSet<_> = covered.iter().copied().collect();
                let first_set: HashSet<_> = first_covered.iter().copied().collect();
                assert_eq!(
                    covered_set, first_set,
                    "round {round}: the covered ids stay the same fixed hot window"
                );
                assert_ne!(
                    &token, first_token,
                    "round {round}: each plain search starts its own walk identity"
                );
            }
        }
    }
}

/// V4 修复的正向面：有效续查每次推进且不漏页（与 S3 既有用例互补，这里
/// 同时钉住「去重后集合单调但不含重复」）。
#[tokio::test]
async fn a_valid_continuation_advances_without_duplicates_or_missed_pages() {
    let dir = tempfile::tempdir().unwrap();
    let (value, ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value).await.unwrap();

    let mut seen: HashSet<ContextItemId> = engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .expect("the first window matches")
        .iter()
        .map(|hit| hit.item_id)
        .collect();
    let mut token = engine
        .last_search_coverage()
        .continuation
        .expect("the first pass is incomplete and issues a token");
    let expected: HashSet<_> = ids.into_iter().collect();
    for pass in 0..8 {
        let page = engine
            .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &token)
            .await
            .unwrap_or_else(|e| {
                panic!("pass {pass}: a query-bound continuation serves its page: {e}")
            });
        let before = seen.len();
        seen.extend(page.iter().map(|hit| hit.item_id));
        assert!(
            seen.len() > before,
            "pass {pass}: a valid continuation must advance to unseen pages"
        );
        if seen == expected {
            break;
        }
        let (_token, covered) = engine
            .search_continuation_probe()
            .expect("the walk stays live while pages remain");
        let covered: HashSet<_> = covered.into_iter().collect();
        assert!(
            covered.len() <= expected.len(),
            "pass {pass}: the deduped covered set never exceeds the history"
        );
        token = engine
            .last_search_coverage()
            .continuation
            .unwrap_or_else(|| panic!("pass {pass}: an incomplete pass issues the next token"));
    }
    assert_eq!(
        seen, expected,
        "the continuation walk reaches every cold page exactly once"
    );
}

/// V5 反例（红-first）：视图 V1（已推进两步的遍历）签发 token → 原位恢复
/// V0 → 旧 token 被拒：不得把 V0 未搜索内容当已覆盖。旧代码 restore 不清
/// 理续查状态，旧 token 命中后用旧覆盖集合 skip 掉 V0 里从未搜索过的页。
#[tokio::test]
async fn restore_invalidates_stale_continuation_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value.clone()).await.unwrap();

    // 遍历第一步：fresh 窗口 W1（restore 批次装进的前 4 条）。
    let hits = engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .unwrap();
    assert_eq!(hits.len(), CAP);
    let (t1, c1) = engine.search_continuation_probe().expect("walk started");
    assert_eq!(c1.len(), CAP, "the first walk covers exactly its window");

    // 遍历第二步：消费 t1，覆盖集合并入第二个窗口（V1 视图）。
    engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("the live chain advances");
    let (t2, c2) = engine.search_continuation_probe().expect("walk advanced");
    assert_eq!(
        c2.len(),
        2 * CAP,
        "the resumed walk accumulates two windows"
    );
    assert_ne!(t1, t2);

    // 原位恢复 V0（同一份更早的 checkpoint）：旧 token 全部失效。
    engine.restore(value).await.unwrap();
    assert!(
        engine.search_continuation_probe().is_none(),
        "a successful in-place restore must clear the continuation state"
    );

    // 旧 token 到达：明确拒绝为续查，本次 pass 按 fresh 遍历执行——只覆盖
    // 它自己看到的窗口，不继承旧覆盖集合，不把 V0 未搜索内容当已覆盖。
    let hits = engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t2)
        .await
        .expect("a rejected token degrades to a fresh search, not an error");
    assert!(!hits.is_empty());
    let (t3, c3) = engine
        .search_continuation_probe()
        .expect("the fresh pass issues its own walk");
    assert_ne!(t3, t1, "the fresh walk does not reuse a consumed identity");
    assert_ne!(
        t3, t2,
        "the fresh walk does not reuse a pre-restore identity"
    );
    assert_eq!(
        c3.len(),
        CAP,
        "the rejected token must not mark V0's un-searched pages as covered"
    );
    let covered: HashSet<_> = c3.into_iter().collect();
    assert_eq!(
        covered,
        carded_hot_ids(&engine).await,
        "the fresh walk's covered set is exactly the window it examined"
    );
}

/// V5 反例（红-first）：链完成后新链不得复用旧编号身份。旧代码 token 由
/// slot 尾数推导（cold-window-N），链完成清空后下一个链从 1 重新计数——
/// 不同链签发出相同 token。
#[tokio::test]
async fn a_completed_chains_tokens_are_never_reused_by_a_new_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value).await.unwrap();

    // 链 A 走查询 alpha 到完整；链 B 是同一引擎上的新查询。新链的任何
    // token 不得与链 A 的重复。
    let chain_a = walk_collecting_tokens(&engine, "unique-token-alpha").await;
    assert!(
        !chain_a.is_empty(),
        "chain A must issue at least one token before completing"
    );
    assert!(engine.search_continuation_probe().is_none());

    let chain_b = walk_collecting_tokens(&engine, "unique-token-beta").await;
    assert!(!chain_b.is_empty());
    let overlap: Vec<_> = chain_a.intersection(&chain_b).collect();
    assert!(
        overlap.is_empty(),
        "a new chain must never reuse a completed chain's token identities: {overlap:?}"
    );
}

/// 把一个查询的 continuation 链走到完整，收集沿途签发的全部 token。
async fn walk_collecting_tokens(engine: &SimpleContextEngine, query: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let search = ContextSearchQuery::new(query, 20);
    // 该查询可能命中（Ok）也可能在空结果上 fail-closed（Err）——两种 pass
    // 都记录 coverage 并可能在未完整时签发 token。
    let _ = engine.search_external(search.clone()).await;
    for _ in 0..16 {
        let coverage = engine.last_search_coverage();
        if coverage.complete {
            break;
        }
        let token = coverage
            .continuation
            .expect("an incomplete pass issues a token");
        tokens.insert(token.clone());
        let _ = engine
            .search_external_continuation(search.clone(), &token)
            .await;
    }
    tokens
}

/// V4 链级上限：一条遍历的覆盖集合超过配置上限时，链以显式状态关闭——
/// typed stop（Budget）＋无 continuation、覆盖状态释放；紧随其后的普通
/// 搜索开始新链并照常签发 token（可继续，不漏可达性）。
#[tokio::test]
async fn a_walk_past_its_chain_bound_closes_with_an_explicit_stop_and_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // 上限钉在两个窗口之间：第一步覆盖 4，resume 并入第二个窗口后
        // （8 > 5）链必须在第二步关闭。
        search_continuation_max_covered_ids: 2 * CAP - 3,
        ..tight_config(&dir, CAP)
    });
    engine.restore(value).await.unwrap();

    engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .unwrap();
    let (t1, c1) = engine.search_continuation_probe().expect("walk started");
    assert_eq!(c1.len(), CAP);

    // 越界的 resume：pass 本身照常服务命中，但链关闭、不再签发 token。
    let page = engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("the over-bound pass still serves its hits");
    assert!(!page.is_empty());
    let coverage = engine.last_search_coverage();
    assert!(!coverage.complete, "pages remain unread: {coverage:?}");
    assert_eq!(
        coverage.stop,
        ContextSearchCoverageStop::Budget,
        "the chain bound surfaces as the typed items-budget stop: {coverage:?}"
    );
    assert!(
        coverage.continuation.is_none(),
        "a closed chain issues no continuation token: {coverage:?}"
    );
    assert!(
        engine.search_continuation_probe().is_none(),
        "the over-bound walk's retained state is released"
    );

    // 立即用普通搜索重启：新链照常建立（fresh 不继承，也不受旧链影响）。
    let hits = engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .expect("a fresh search restarts the walk");
    assert!(!hits.is_empty());
    let (t2, c2) = engine
        .search_continuation_probe()
        .expect("a new chain starts");
    assert_ne!(t2, t1);
    assert_eq!(c2.len(), CAP, "the new chain covers only its own window");
}

/// V5 守卫：拒绝 restore 不破坏现有合法链。restore 在结构校验失败时整体
/// 拒绝、不落盘任何状态替换——已签发的 token 与已建立的遍历保持有效。
#[tokio::test]
async fn a_rejected_restore_keeps_the_live_chain_valid() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value).await.unwrap();

    engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .unwrap();
    let (t1, c1) = engine.search_continuation_probe().expect("walk started");

    // 非法 checkpoint：restore 必须失败，且不改变现有合法状态。
    engine
        .restore(serde_json::Value::Null)
        .await
        .expect_err("a structurally invalid checkpoint must be rejected");
    let (t1_after, c1_after) = engine.search_continuation_probe().expect("chain survives");
    assert_eq!(t1_after, t1, "a rejected restore leaves the token valid");
    assert_eq!(c1_after, c1);

    // 该链继续推进。
    let page = engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("the pre-rejection chain still advances");
    assert!(!page.is_empty());
    let (_t2, c2) = engine.search_continuation_probe().expect("walk advanced");
    assert!(
        c2.len() > c1.len(),
        "the surviving walk keeps accumulating pages"
    );
}

/// V5 幂等面：同一 token 在被消费后重复到达，语义必须稳定——明确按 fresh
/// 遍历处理（不是错误、不混入不同遍历状态），且重复到达结果确定。
#[tokio::test]
async fn repeating_an_already_consumed_token_degrades_to_a_fresh_walk() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids) = carded_history(&dir, N).await;
    let engine = SimpleContextEngine::new(tight_config(&dir, CAP));
    engine.restore(value).await.unwrap();

    engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .unwrap();
    let (t1, _c1) = engine.search_continuation_probe().expect("walk started");
    engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("the first use of t1 advances the walk");
    let (_t2, c2) = engine.search_continuation_probe().expect("walk advanced");

    // t1 第二次到达（已被 t2 取代）：不报错、不旋转 t2 的窗口、不继承 c2。
    let replay = engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("an already-consumed token degrades to a fresh search, not an error");
    assert!(!replay.is_empty());
    let (t3, c3) = engine
        .search_continuation_probe()
        .expect("the fresh pass issues its own walk");
    assert_ne!(t3, t1);
    let c3_set: HashSet<_> = c3.into_iter().collect();
    let c2_set: HashSet<_> = c2.into_iter().collect();
    assert_ne!(
        c3_set, c2_set,
        "the replay must not mix the replaced walk's covered state"
    );
    assert_eq!(
        c3_set,
        carded_hot_ids(&engine).await,
        "the replay's covered set is exactly the window it examined itself"
    );

    // 再重复一次同样的 t1：同样的 fresh 语义，结果确定。
    let replay_again = engine
        .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &t1)
        .await
        .expect("the replay is repeatable and stable");
    assert_eq!(
        replay.len(),
        replay_again.len(),
        "repeating the same stale token is semantically stable"
    );
}

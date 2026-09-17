//! S3（2026-09-15 continuation review R3/R4）：固定预算下的冷目录闭环。
//!
//! R3：「有界」不能只是批间检查——单次读取要受剩余 deadline 约束；批 take
//! 要按剩余字节预留；per-id fetch/inspect 要走与 GC 相同的驻留结算入口，
//! 固定热上限下顺序遍历多倍于预算的历史仍可取回全部正文或如实背压。
//! R4：非空搜索不得丢掉 coverage——typed 覆盖事实 + 绑定查询的
//! continuation 续查推进后续冷页；一个坏页不得永久挡住后续可读页。
//!
//! 五个验收反例（对应完成标准 1–5）：
//! 1. `a_single_slow_card_read_cannot_outlive_the_operation_deadline`
//! 2. `an_entry_larger_than_the_remaining_byte_room_stays_a_pending_owner`
//! 3. `sequential_fetches_over_a_fixed_hot_cap_page_the_whole_history`
//! 4. `search_coverage_names_the_gap_and_a_continuation_reaches_later_pages`
//!    ＋ `a_corrupt_page_does_not_block_the_continuation_walk`
//! 5. `demote_overflow_reports_residual_backpressure…`（external 单元）
//!    ＋ `gc_reports_typed_hot_metadata_backpressure_when_nothing_can_demote`

use std::sync::Arc;

use agent_contracts::{
    ContextEngine, ContextItemId, ContextKind, ContextResidency, ContextRetention, ContextScope,
    ContextSearchCoverageStop, ContextSearchQuery,
};

use crate::engine::{HydrationBudget, HydrationStop, SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 统一的预算构造：条数/字节上限显式给定，deadline 远期（除慢读反例外
/// 不约束时钟）。
fn budget(max_items: usize, hot_max_entries: usize, hot_max_bytes: u64) -> HydrationBudget {
    HydrationBudget {
        max_items,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        hot_max_entries,
        hot_max_bytes,
    }
}

/// Externalize `n` 条笔记（空 eviction buffer → 直接入 store），checkpoint
/// 后全部成为卡片（inline 目标 0），residency 钉在 External。正文带序号
/// token，summary 统一垫到同长，使逐条字节估算一致。
async fn spill_history(engine: &SimpleContextEngine, n: usize) -> Vec<ContextItemId> {
    let mut ids = Vec::new();
    for index in 0..n {
        {
            let mut state = engine.state.lock().await;
            let mut body = format!("spill body {index} unique-token-{index} ");
            while body.chars().count() < 121 {
                body.push('p');
            }
            let mut item = crate::item::make_item(
                &state,
                &engine.config,
                body,
                ContextKind::Note,
                ContextScope::Task,
                ContextRetention::Working,
                0.5,
                Some("fixed-budget".into()),
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

/// 建立一段全部卡片化的历史，返回 (checkpoint, ids, 每条字节估算)。
async fn carded_history(
    dir: &tempfile::TempDir,
    n: usize,
) -> (serde_json::Value, Vec<ContextItemId>, u64) {
    let first = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        // capture 的卡片写入有墙钟预算（超时条目留在 inline、下次 capture
        // 续写）：满载 CI runner 上 14 次小文件写入（探测读＋fsync＋rename）
        // 可越过默认 2s（run 35158964457：spilled 12/14），墙钟噪声不是本
        // 模块钉住的对象，这里把它钉宽。
        external_checkpoint_io_budget_ms: 60_000,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&first, "fixed budget closure").await;
    let ids = spill_history(&first, n).await;
    let value = first.checkpoint().await.unwrap();
    let spilled = value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .expect("inline target 0 spills the whole tail")
        .len();
    assert_eq!(spilled, n, "every cold entry is a manifest row");
    let total = first.state.lock().await.external.metadata_bytes_estimate();
    let per = total / n as u64;
    assert_eq!(
        per * n as u64,
        total,
        "setup: the uniform bodies make per-entry estimates identical"
    );
    (value, ids, per)
}

/// 验收反例 1（R3 时间边界，红-first）：挂在读边界的单次冷读取不得越过
/// 操作 deadline。旧形状在读取 await 点无期限——操作永不返回；新形状在
/// 剩余 deadline 处取消等待，该行保持 pending owner，操作以类型化超时
/// 事实返回，且已装条目结算完整。
#[tokio::test]
async fn a_single_slow_card_read_cannot_outlive_the_operation_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids, _per) = carded_history(&dir, 4).await;
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 1,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }));
    engine.restore(value).await.unwrap();
    assert_eq!(engine.state.lock().await.pending_external_cards.len(), 3);

    // 把第一次卡片读取钉在 pause 边界，且永不放行：一次读取比剩余 deadline
    // 慢得多。
    let planned = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *engine
        .card_read_pause
        .lock()
        .expect("card read pause mutex poisoned") = Some((
        crate::engine::IoBoundaryPause {
            planned: Arc::clone(&planned),
            release: Arc::clone(&release),
        },
        0,
    ));

    let started = std::time::Instant::now();
    let slow_read_budget = HydrationBudget {
        max_items: 10,
        deadline: started + std::time::Duration::from_millis(400),
        hot_max_entries: usize::MAX,
        hot_max_bytes: u64::MAX,
    };
    let hydrator = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.hydrate_within_budget(slow_read_budget, &[]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), planned.notified())
        .await
        .expect("the drain reached the first card-read boundary");
    // 旧形状在这里永远等不到 release；操作必须自己按剩余 deadline 返回。
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), hydrator)
        .await
        .expect("the slow read must be cancelled at the deadline, not waited out")
        .expect("the drain task did not panic");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the operation returned on its own deadline, not by test timeout"
    );

    // 超时事实类型化为 Deadline；该行 pending owner 不变，无半装状态。
    assert_eq!(
        outcome.stopped,
        HydrationStop::Deadline,
        "the typed stop cause is the cancelled slow read: {outcome:?}"
    );
    assert_eq!(outcome.remaining, 3, "the timed-out row keeps its owner");
    {
        let state = engine.state.lock().await;
        assert_eq!(state.pending_external_cards.len(), 3);
        assert_eq!(
            state.external.len(),
            1,
            "no half-finished install committed"
        );
        assert_eq!(state.external_card_io_failures, 0);
    }

    // 放行并清理 pause：下一次 drain 正常完成，队列排空——无永久损伤。
    *engine.card_read_pause.lock().expect("poisoned") = None;
    release.notify_one();
    let outcome = engine
        .hydrate_within_budget(budget(100, usize::MAX, u64::MAX), &[])
        .await;
    assert!(
        outcome.complete,
        "the recovered drain finishes: {outcome:?}"
    );
    assert_eq!(engine.state.lock().await.pending_external_cards.len(), 0);
}

/// 验收反例 2（R3 字节边界，红-first）：热字节上限只剩少量余量时，一批
/// 大元数据条目不得安装后越界。装不下的条目保持 pending 可寻址（按 id
/// fetch 仍取回正文），热表不越字节上限。
#[tokio::test]
async fn an_entry_larger_than_the_remaining_byte_room_stays_a_pending_owner() {
    let dir = tempfile::tempdir().unwrap();
    let n = 6;
    let (value, ids, per) = carded_history(&dir, n).await;
    // 字节上限只装得下 2.5 条：restore 批 2 条后，剩余 room ≈ 0.5 条，
    // 任何一条完整条目都装不下。
    let byte_cap = per * 2 + per / 2;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 2,
        external_hot_metadata_max_entries: 4096,
        external_hot_metadata_max_bytes: byte_cap,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.external.len(), 2, "setup: restore pages one batch in");
        assert_eq!(state.pending_external_cards.len(), 4);
    }

    let outcome = engine
        .hydrate_within_budget(budget(100, 4096, byte_cap), &[])
        .await;
    {
        let state = engine.state.lock().await;
        let bytes = state.external.metadata_bytes_estimate();
        assert!(
            bytes <= byte_cap,
            "the hot map must not cross the byte cap: {bytes} > {byte_cap} ({outcome:?})"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            4,
            "every oversized entry keeps its addressable cold owner row"
        );
    }
    // 类型化事实：容量停止，而不是 Unreadable 也不是假装完整。
    assert_eq!(
        outcome.stopped,
        HydrationStop::HotCap,
        "no room for any read entry is a capacity stop: {outcome:?}"
    );

    // 大条目保持可寻址：按 id fetch 取回正文（per-id 结算把最旧的已卡片化
    // 条目换回 pending，热表仍在字节上限内）。
    let big_id = ids[2];
    let fetched = engine.fetch_external(big_id).await.unwrap().unwrap();
    assert!(
        fetched.content.contains("unique-token-2"),
        "the oversized entry's body is still reachable: {}",
        fetched.content
    );
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.metadata_bytes_estimate() <= byte_cap,
            "the per-id settle keeps the byte cap"
        );
    }
}

/// 验收反例 3（R3 全入口边界，红-first）：热上限固定为 C，顺序
/// fetch/inspect 3C+ 条历史——每条正文都取回（热↔冷换页），结束时热条数
/// 受控、owner 集合守恒，全程无未报告的超限。这个反例替代「提高上限排空
/// 全历史」的旧形状。
#[tokio::test]
async fn sequential_fetches_over_a_fixed_hot_cap_page_the_whole_history() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 4usize;
    let n = 3 * cap + 2;
    let (value, ids, _per) = carded_history(&dir, n).await;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 1,
        external_hot_metadata_max_entries: cap,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value).await.unwrap();

    for (index, id) in ids.iter().enumerate() {
        let fetched = engine
            .fetch_external(*id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("fetch {index} must serve the body"));
        assert!(
            fetched.content.contains(&format!("unique-token-{index}")),
            "fetch {index} returns the captured body"
        );
        let hot = engine.state.lock().await.external.len();
        assert!(
            hot <= cap,
            "the hot directory stays within the fixed cap across the whole walk \
             (after fetch {index}: {hot})"
        );
    }
    // inspect 走同一 per-id 服务与结算。
    let inspected = engine
        .inspect_external(ids[cap * 2])
        .await
        .unwrap()
        .expect("inspect resolves a paged id");
    let _ = inspected;

    // owner 集合守恒：热 ∪ pending == 全历史，且不相交。
    {
        let state = engine.state.lock().await;
        let hot: std::collections::HashSet<_> = state.external.iter().map(|e| e.item_id).collect();
        let pending: std::collections::HashSet<_> = state
            .pending_external_cards
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert!(hot.len() <= cap, "final hot residency is bounded");
        assert!(
            hot.is_disjoint(&pending),
            "hot and pending must not overlap"
        );
        let mut owners = hot.clone();
        owners.extend(&pending);
        let expected: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(
            owners, expected,
            "paging never loses or duplicates an owner"
        );
    }
}

/// 验收反例 4（R4，红-first）：1 个窗口命中 + 大量未读冷页 + limit 20 →
/// typed coverage 携带不完整事实与可续查 continuation；用 continuation
/// 续查真正推进到后续冷页（不反复撞同一满员热表）；走完时零命中才是
/// 合法结果。
#[tokio::test]
async fn search_coverage_names_the_gap_and_a_continuation_reaches_later_pages() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 4usize;
    let n = 14;
    let (value, ids, _per) = carded_history(&dir, n).await;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: cap,
        external_hot_metadata_max_entries: cap,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value).await.unwrap();

    let hits = engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .expect("non-empty partial hits are returned, not dropped");
    assert_eq!(hits.len(), cap, "only the first window is visible");
    let coverage = engine.last_search_coverage();
    assert!(
        !coverage.complete,
        "10 unread pages must not read as complete coverage: {coverage:?}"
    );
    assert_eq!(coverage.unread_pages, n - cap);
    assert_eq!(coverage.stop, ContextSearchCoverageStop::HotCap);
    let first_token = coverage
        .continuation
        .clone()
        .expect("an incomplete search issues a continuation");

    // 续查推进：每一步拿到新的冷页，热目录不越固定上限，已见集合单调
    // 增长，直到覆盖事实转为 complete。
    let mut seen: std::collections::HashSet<_> = hits.iter().map(|h| h.item_id).collect();
    let mut token = first_token;
    let mut passes = 0;
    loop {
        passes += 1;
        assert!(passes <= 6, "the walk must converge, not cycle: {seen:?}");
        let page = engine
            .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &token)
            .await
            .expect("a query-bound continuation serves the next page");
        let fresh: Vec<_> = page
            .iter()
            .filter(|hit| !seen.contains(&hit.item_id))
            .collect();
        assert!(
            !fresh.is_empty(),
            "a continuation must advance to unseen cold pages, pass {passes}"
        );
        seen.extend(page.iter().map(|hit| hit.item_id));
        let hot = engine.state.lock().await.external.len();
        assert!(hot <= cap, "the fixed cap holds through the walk: {hot}");
        let coverage = engine.last_search_coverage();
        if coverage.complete {
            break;
        }
        token = coverage
            .continuation
            .clone()
            .expect("an incomplete pass issues the next continuation");
    }
    let expected: std::collections::HashSet<_> = ids.iter().copied().collect();
    assert_eq!(
        seen, expected,
        "the continuation walk reaches every cold page"
    );

    // 一个新查询的零命中在热上限处依旧 fail-closed（B2 语义保留），错误里
    // 带出自己的 continuation；把这个查询自己的候选区域也走完，完整零命中
    // 才是合法的 Ok。
    let zero_error = engine
        .search_external(ContextSearchQuery::new("zzz-no-such-token", 20))
        .await
        .expect_err("a capped zero-match must fail closed");
    assert!(
        zero_error.to_string().contains("continuation="),
        "the fail-closed error names the way forward: {zero_error}"
    );
    let mut token = engine
        .last_search_coverage()
        .continuation
        .expect("the failed-closed pass recorded its coverage facts");
    let mut passes = 0;
    loop {
        passes += 1;
        assert!(passes <= 6, "the zero-match walk must converge");
        let outcome = engine
            .search_external_continuation(ContextSearchQuery::new("zzz-no-such-token", 20), &token)
            .await;
        if outcome.is_ok() {
            break;
        }
        let coverage = engine.last_search_coverage();
        assert!(
            !coverage.complete,
            "an incomplete zero-match stays fail-closed: {coverage:?}"
        );
        token = coverage
            .continuation
            .expect("an incomplete pass issues the next continuation");
    }
    assert!(
        engine.last_search_coverage().complete
            || engine.last_search_coverage().continuation.is_none(),
        "the walk ended with complete coverage facts"
    );
}

/// 验收反例 4 的坏页变体：一个不可解码的坏页不得永久挡住后续可读页——
/// 它被如实计数（missing），后续页仍可达，coverage 事实如实。
#[tokio::test]
async fn a_corrupt_page_does_not_block_the_continuation_walk() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 4usize;
    let n = 14;
    let (value, ids, _per) = carded_history(&dir, n).await;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: cap,
        external_hot_metadata_max_entries: cap,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value).await.unwrap();

    // 毁掉一个尚未读入的卡片（pending 目录第一行）。
    let (bad_id, bad_hash) = {
        let state = engine.state.lock().await;
        state.pending_external_cards[0].clone()
    };
    let card = dir
        .path()
        .join("cards")
        .join(format!("{bad_id}.{bad_hash}.card"));
    std::fs::write(&card, b"garbage that no longer matches the captured hash").unwrap();

    let hits = engine
        .search_external(ContextSearchQuery::new("unique-token", 20))
        .await
        .unwrap();
    let mut seen: std::collections::HashSet<_> = hits.iter().map(|h| h.item_id).collect();
    assert!(
        !seen.contains(&bad_id),
        "the corrupt page has no metadata to hit"
    );
    let mut token = engine
        .last_search_coverage()
        .continuation
        .clone()
        .expect("the first pass is incomplete");
    let mut passes = 0;
    loop {
        passes += 1;
        assert!(passes <= 6, "the bad page must not stall the walk");
        let page = engine
            .search_external_continuation(ContextSearchQuery::new("unique-token", 20), &token)
            .await
            .unwrap();
        seen.extend(page.iter().map(|hit| hit.item_id));
        let coverage = engine.last_search_coverage();
        if coverage.complete {
            break;
        }
        token = coverage
            .continuation
            .clone()
            .expect("an incomplete pass issues the next continuation");
    }
    let expected: std::collections::HashSet<_> = ids.iter().copied().collect();
    let mut reachable = expected.clone();
    reachable.remove(&bad_id);
    assert!(
        seen.is_superset(&reachable),
        "every readable page stays reachable past the bad page: missing {:?}",
        reachable.difference(&seen).collect::<Vec<_>>()
    );
    assert!(
        !seen.contains(&bad_id),
        "the corrupt page never pretends to have been read"
    );
    assert!(
        engine.state.lock().await.external_cards_missing >= 1,
        "the undecodable page is counted honestly"
    );
}

/// 验收反例 5（引擎层）：热目录超预算且无任何可降级条目（无卡片、含
/// pinned）→ GC 报告携带类型化背压事实，不是静默成功。
#[tokio::test]
async fn gc_reports_typed_hot_metadata_backpressure_when_nothing_can_demote() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        // 默认 inline 目标：外置条目不写卡片（无 claim 可降级）。
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "backpressure").await;
    let ids = spill_history(&engine, 3).await;
    // 收紧热上限到 2：三条无卡片条目必然超预算且无可降级项。
    let mut engine = engine;
    engine.config.external_hot_metadata_max_entries = 2;
    let report = engine.gc().await.unwrap();
    assert!(
        report.hot_metadata_backpressure,
        "over budget with nothing demotable must be reported, not passed silently: {report:?}"
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.external.len(),
            3,
            "owners are never dropped to enforce the cap"
        );
    }
    let _ = ids;
}

/// 验收反例 5（补）：有可降级条目时同一入口把热表收回预算内，报告如实
/// 显示无背压。
#[tokio::test]
async fn gc_settles_back_within_the_cap_when_demotable_entries_exist() {
    let dir = tempfile::tempdir().unwrap();
    let (value, _ids, _per) = carded_history(&dir, 6).await;
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 6,
        external_hot_metadata_max_entries: 4,
        gc_buffer_capacity: 0,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    engine.restore(value).await.unwrap();
    assert_eq!(
        engine.state.lock().await.external.len(),
        6,
        "setup: all hot"
    );
    let report = engine.gc().await.unwrap();
    assert!(
        !report.hot_metadata_backpressure,
        "demotable carded entries bring the map back within the cap: {report:?}"
    );
    assert!(
        engine.state.lock().await.external.len() <= 4,
        "the settled map is within the cap"
    );
}

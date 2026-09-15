//! W1（2026-09-16 review 4f6eb7ff，V1＋V2）：逻辑 owner 与冷页解析。
//!
//! V1：退休扫描的 referenced 集合只枚举 heap/Warm/写入重试表/已加载
//! external/active scope——未加载 pending 卡片中的 scope 引用不可见。
//! 反例链：合法卡片 A 引用已关闭 scope S → A 留在 pending → scope 数超退休
//! 阈值 → 退休扫描看不到 A、移除 S → 按 ID 读 A 时 hydrate_card_for 因 S
//! 不存在而消费定位行 → 可达性/恢复目录被破坏。
//!
//! V2：`plan_required` 只查已加载索引——同一正文 `fetch_external(id)` 可读、
//! 声明成 PromptRequired 却报 Missing。必需正文规划前要对有界 required refs
//! 做目标解析（精确 ID 直接定位 pending 卡片读回；实体/路径沿冷目录有界
//! 扫描），且区分 不存在/读取失败/因预算暂未解析，不压成 Missing。
//!
//! 红线反例都在 HEAD 上先红（fetch 消费定位行 / required_misses 报
//! Missing / 首轮 GC 立即退休），实现后转绿。

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextEngine, ContextItemId, ContextKind,
    ContextMaterializationMissReason, ContextQuery, ContextResidency, ContextRetention,
    ContextScope, RootReason, ScopeId, ScopeKind,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 与 cold_bounds 同形的固定预算配置：全部冷条目卡片化（inline 目标 0）、
/// restore 只读一个批次、bulk 水化每操作条数受限、热目录上限固定。
fn w1_config(
    dir: &tempfile::TempDir,
    restore_batch: usize,
    hydrate_items: usize,
) -> SimpleContextConfig {
    SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: restore_batch,
        external_hydrate_max_items: hydrate_items,
        external_hot_metadata_max_entries: 64,
        gc_buffer_capacity: 0,
        // 捕获卡片写入的墙钟预算钉宽（CI 噪声不是本模块的对象）。
        external_checkpoint_io_budget_ms: 60_000,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

/// Externalize one note stamped with `scope_id` (empty eviction buffer →
/// straight to the store) and pin its residency at External so the spill
/// gate sees it. Returns the item id (the external entry's id).
async fn externalize_in_scope(
    engine: &SimpleContextEngine,
    scope_id: Option<ScopeId>,
    body: &str,
) -> ContextItemId {
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
            Some("w1".into()),
        );
        item.scope_id = scope_id;
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

/// The pending row for `id`, proving the card was NOT loaded this pass (the
/// id is known, the metadata is not in memory).
async fn pending_row_for(engine: &SimpleContextEngine, id: ContextItemId) -> Option<String> {
    let state = engine.state.lock().await;
    state
        .pending_external_cards
        .iter()
        .find(|(row_id, _)| *row_id == id)
        .map(|(_, hash)| hash.clone())
}

// ---------------------------------------------------------------------------
// V1 — retirement must not remove a scope an unread pending card references
// ---------------------------------------------------------------------------

/// V1 反例（红-first）：冷卡片引用已关闭 scope → 留在 pending（证明本轮未
/// 加载）→ 超过退休阈值 → GC → 按 ID fetch 必须成功（定位行不被消费、
/// 正文/scope 关系/owner 集合保持）→ checkpoint/restore 后仍可读。
///
/// 构造：A-engine 在 tool scope S（开）内外置 A（卡片携带 scope_id=S）；
/// checkpoint 后 B-engine 恢复（restore 批次只装 fillers，A 留 pending）；
/// B 关闭 S——close 只重标记已加载条目，A 的卡片合法地继续引用 S；GC 的
/// bulk 水化条数为 0（A 可证未加载）；退休阈值 1。旧代码在此退休 S，随后
/// fetch 消费 A 的定位行返回 None——红线即 fetch 断言。
#[tokio::test]
async fn gc_retirement_keeps_a_scope_referenced_by_an_unread_pending_card() {
    let dir = tempfile::tempdir().unwrap();
    // restore 装前 2 行（fillers），A 在尾段；GC 的 bulk 水化一条不读。
    let config = w1_config(&dir, 2, 0);
    let source = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 1,
        ..config.clone()
    });
    open_focus(&source, "logical owner semantics probe").await;
    let (scope_s, _fillers) = {
        let scope_s = source.open_scope(ScopeKind::Tool, None).await.unwrap();
        // fillers 先外置（manifest 前排，restore 首批装载），A 最后（pending 尾）。
        let f1 = externalize_in_scope(&source, None, "payload-alpha-one").await;
        let f2 = externalize_in_scope(&source, None, "payload-alpha-two").await;
        let target = externalize_in_scope(&source, Some(scope_s), "payload-omega-target").await;
        (scope_s, vec![f1, f2, target])
    };
    let target = _fillers[2];
    let checkpoint = source.checkpoint().await.unwrap();
    {
        let spilled = checkpoint
            .get("external_spilled")
            .and_then(|v| v.as_array())
            .expect("inline target 0 spills the whole tail")
            .len();
        assert_eq!(spilled, 3, "every cold entry is a manifest row");
    }

    // 冷恢复：S 仍在树中（checkpoint 于 S 关闭前捕获）。
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 1,
        ..config
    });
    engine.restore(checkpoint).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.scopes.by_id(scope_s).is_some(),
            "setup: the tool scope restored into the tree"
        );
        assert_eq!(state.external.len(), 2, "restore pages exactly one batch");
        assert_eq!(state.pending_external_cards.len(), 1);
    }
    // 目标卡片本轮未加载：不是 fixture 顺序巧合，而是断言过的批次边界。
    assert!(pending_row_for(&engine, target).await.is_some());

    // 关闭 S：close 只重标记已加载条目；未加载卡片合法地继续引用 S。
    engine.close_scope(scope_s).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state
                .scopes
                .by_id(scope_s)
                .is_some_and(|s| s.state == agent_contracts::ScopeState::Closed),
            "setup: the scope is closed and above the retirement gate"
        );
    }

    // GC：bulk 水化一条不读，A 保持 pending（可证未加载）。
    engine.gc().await.unwrap();
    assert!(
        pending_row_for(&engine, target).await.is_some(),
        "the target card must still be a pending row after the GC pass"
    );

    // 按 ID fetch：正文可读、定位行不被消费。旧代码在此退休 S 后消费定位行
    // 返回 None——红线。
    let fetched = engine.fetch_external(target).await.unwrap();
    let fetched =
        fetched.expect("an unread pending card referencing a closed scope must still fetch by id");
    assert!(
        fetched.content.contains("payload-omega-target"),
        "the fetched body is the captured one: {}",
        fetched.content
    );
    // scope 关系与 owner 集合保持：条目安装后引用 S，S 仍在树中，owner 唯一。
    {
        let state = engine.state.lock().await;
        assert!(
            state.scopes.by_id(scope_s).is_some(),
            "the closed scope survives retirement while its cold card references it"
        );
        let entry = state.external.get(target).expect("the entry installed");
        assert_eq!(entry.scope_id, Some(scope_s));
        let owners = usize::from(state.items.indexes().get(target).is_some())
            + usize::from(state.eviction_buffer.iter().any(|i| i.id == target))
            + usize::from(
                state
                    .pending_externalize_retry
                    .iter()
                    .any(|i| i.id == target),
            )
            + usize::from(state.external.get(target).is_some());
        assert_eq!(owners, 1, "exactly one residency owner after the fetch");
    }

    // checkpoint/restore 后仍可读。
    let second = engine.checkpoint().await.unwrap();
    let restored = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 1,
        ..w1_config(&dir, 2, 0)
    });
    restored.restore(second).await.unwrap();
    let fetched = restored.fetch_external(target).await.unwrap();
    let fetched = fetched.expect("the card stays readable across checkpoint/restore");
    assert!(fetched.content.contains("payload-omega-target"));
}

/// V1 收敛（红-first）：固定预算下多次 GC（pending 逐步被消费后）退休恢复
/// 推进——不许永久卡死正常退休。旧代码首轮 GC 即退休（无保守推迟），本测
/// 对「未证明闭包完整时先不退休」的红线在首轮断言上。
#[tokio::test]
async fn gc_retirement_resumes_once_the_pending_queue_drains() {
    let dir = tempfile::tempdir().unwrap();
    // 7 张卡片；restore 装 2、pending 5；每轮 GC 的水化/探测条数 2。
    let config = w1_config(&dir, 2, 2);
    let source = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 1,
        ..config.clone()
    });
    open_focus(&source, "convergence semantics probe").await;
    // T：不被任何条目/卡片引用的已关闭 tool scope——正常退休的对象。
    let scope_t = source.open_scope(ScopeKind::Tool, None).await.unwrap();
    for index in 0..7 {
        externalize_in_scope(&source, None, &format!("payload-delta-{index}")).await;
    }
    source.close_scope(scope_t).await.unwrap();
    let checkpoint = source.checkpoint().await.unwrap();

    let engine = SimpleContextEngine::new(SimpleContextConfig {
        scope_retire_target: 1,
        ..config
    });
    engine.restore(checkpoint).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.pending_external_cards.len(), 5);
        assert!(
            state.scopes.by_id(scope_t).is_some(),
            "setup: the closed unreferenced scope restored"
        );
    }

    // 首轮：pending 未排空且探测也读不完 → 保守推迟（旧代码：立即退休——红）。
    let report = engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.scopes.by_id(scope_t).is_some(),
            "while the pending cold directory is unproven, retirement defers"
        );
        assert_eq!(
            report.scopes_retired, 0,
            "the deferral is honest in the report"
        );
        assert!(
            report.scope_retirement_deferred,
            "the report names the deferral: {report:?}"
        );
    }

    // 收敛：固定预算下重复 GC，pending 逐步被消费；排空后退休恢复推进。
    let mut retired = false;
    for _ in 0..8 {
        engine.gc().await.unwrap();
        let state = engine.state.lock().await;
        if state.scopes.by_id(scope_t).is_none() {
            retired = true;
            break;
        }
    }
    assert!(
        retired,
        "retirement must converge once the pending queue drains — no permanent stall"
    );
}

// ---------------------------------------------------------------------------
// V2 — PromptRequired resolution over pending cold cards
// ---------------------------------------------------------------------------

/// V2 反例（红-first）：目标位于 restore 首批之后（pending 中）＋固定热预算＋
/// 精确 PromptRequired URI → 直接 materialize：正文被提供。旧代码
/// required_misses 报 Missing——红线。
#[tokio::test]
async fn a_prompt_required_ref_to_a_pending_card_is_resolved_and_served() {
    let dir = tempfile::tempdir().unwrap();
    let config = w1_config(&dir, 2, 0);
    let source = SimpleContextEngine::new(config.clone());
    open_focus(&source, "required semantics probe").await;
    let f1 = externalize_in_scope(&source, None, "payload-beta-one").await;
    let f2 = externalize_in_scope(&source, None, "payload-beta-two").await;
    let target = externalize_in_scope(&source, None, "payload-omega-required").await;
    let checkpoint = source.checkpoint().await.unwrap();

    let engine = SimpleContextEngine::new(config);
    engine.restore(checkpoint).await.unwrap();
    assert!(pending_row_for(&engine, target).await.is_some());
    let _ = (f1, f2);

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![AnchorRootClaim {
                    item_ref: format!("context://run/{target}"),
                    strength: AnchorRootStrength::PromptRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 3,
                    reason: RootReason::HardConstraint,
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
            .all(|miss| miss.reason != ContextMaterializationMissReason::Missing),
        "a PromptRequired body that fetch_external can read must not be reported Missing: {:?}",
        materialized.required_misses
    );
    assert!(
        materialized.items.iter().any(|item| item.item_id == target),
        "the resolved pending body must reach the frame: {:?}",
        materialized.items
    );
}

/// V2 原因区分（红-first）：真正不存在的 id 报不存在；读取失败（坏卡）报
/// 读取失败；因预算暂未解析的实体引用不压成 Missing。
#[tokio::test]
async fn required_miss_reasons_distinguish_absent_corrupt_and_unread() {
    let dir = tempfile::tempdir().unwrap();
    let config = w1_config(&dir, 2, 0);
    let source = SimpleContextEngine::new(config.clone());
    open_focus(&source, "reasons semantics probe").await;
    externalize_in_scope(&source, None, "payload-gamma-one").await;
    externalize_in_scope(&source, None, "payload-gamma-two").await;
    let target = externalize_in_scope(&source, None, "payload-omega-reasons").await;
    // 一张不被任何声明命中的尾卡：按 id 解析消费坏卡后它仍未读，
    // 实体引用的缺席结论因此不可证明。
    externalize_in_scope(&source, None, "payload-gamma-tail").await;
    let checkpoint = source.checkpoint().await.unwrap();

    let engine = SimpleContextEngine::new(config);
    engine.restore(checkpoint).await.unwrap();
    let hash = pending_row_for(&engine, target)
        .await
        .expect("the target card stays pending");
    // 坏卡：覆盖卡片文件为垃圾字节。
    let card_path =
        crate::store::external_card_path(&crate::store::store_dir(&engine.config), target, &hash);
    tokio::fs::write(&card_path, b"not a card at all")
        .await
        .unwrap();

    let absent = ContextItemId::new();
    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![
                    AnchorRootClaim {
                        item_ref: format!("context://run/{absent}"),
                        strength: AnchorRootStrength::PromptRequired,
                        source_field_id: "working_refs".into(),
                        anchor_revision: 3,
                        reason: RootReason::HardConstraint,
                    },
                    AnchorRootClaim {
                        item_ref: format!("context://run/{target}"),
                        strength: AnchorRootStrength::PromptRequired,
                        source_field_id: "working_refs".into(),
                        anchor_revision: 3,
                        reason: RootReason::HardConstraint,
                    },
                    AnchorRootClaim {
                        item_ref: "no-such-entity-anywhere".into(),
                        strength: AnchorRootStrength::PromptRequired,
                        source_field_id: "working_refs".into(),
                        anchor_revision: 3,
                        reason: RootReason::HardConstraint,
                    },
                ],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let misses = materialized.required_misses.as_slice();
    let reason_for = |needle: &str| {
        misses
            .iter()
            .find(|miss| miss.identity.item_ref.contains(needle))
            .map(|miss| miss.reason)
    };
    // 真正不存在的 id：Missing（存在性结论成立）。
    assert_eq!(
        reason_for(&absent.to_string()),
        Some(ContextMaterializationMissReason::Missing),
        "a truly absent id is Missing: {misses:?}"
    );
    // 读取失败（坏卡）：Corrupt，不是笼统 Missing（旧代码红）。
    assert_eq!(
        reason_for(&target.to_string()),
        Some(ContextMaterializationMissReason::Corrupt),
        "an unreadable card is a read failure, not absence: {misses:?}"
    );
    // 因预算暂未解析（实体引用＋pending 未读完）：不压成 Missing（旧代码红）。
    let unread = reason_for("no-such-entity-anywhere");
    assert_ne!(
        unread,
        Some(ContextMaterializationMissReason::Missing),
        "an entity ref over an unread cold directory must not claim proven absence: {misses:?}"
    );
    assert_eq!(
        unread,
        Some(ContextMaterializationMissReason::UnreadColdPage),
        "the typed reason names the unread cold pages: {misses:?}"
    );

    // 对照：pending 排空后，同一实体引用的 Miss 才是证明过的零命中。
    let drained = {
        let probe = SimpleContextEngine::new(w1_config(&dir, 2, 64));
        let checkpoint = source.checkpoint().await.unwrap();
        probe.restore(checkpoint).await.unwrap();
        for _ in 0..4 {
            probe.gc().await.unwrap();
        }
        let state = probe.state.lock().await;
        state.pending_external_cards.is_empty()
    };
    assert!(
        drained,
        "control setup: the queue drains under a full budget"
    );
    let control = SimpleContextEngine::new(w1_config(&dir, 2, 64));
    let checkpoint = source.checkpoint().await.unwrap();
    control.restore(checkpoint).await.unwrap();
    for _ in 0..4 {
        control.gc().await.unwrap();
    }
    {
        let state = control.state.lock().await;
        assert!(state.pending_external_cards.is_empty());
    }
    let materialized = control
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![AnchorRootClaim {
                    item_ref: "no-such-entity-anywhere".into(),
                    strength: AnchorRootStrength::PromptRequired,
                    source_field_id: "working_refs".into(),
                    anchor_revision: 3,
                    reason: RootReason::HardConstraint,
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert_eq!(
        materialized.required_misses.as_slice()[0].reason,
        ContextMaterializationMissReason::Missing,
        "with the cold directory fully examined, the zero-match is proven: {:?}",
        materialized.required_misses
    );
}

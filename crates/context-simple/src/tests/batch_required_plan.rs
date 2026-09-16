//! B1（2026-09-16 review 3bdb269c 后 NEXT_TASKS 第五/六批遗留）：多 required
//! 的有界解析计划。
//!
//! W1/V2 的 `resolve_required_cold_refs` 逐个 exact ID 走 per-id lane 装卡，
//! 每次装完立即 `settle_metadata_residency` 且只 protect 刚装的这一个
//! id——热目录上限之下，本批后装的卡会把先装的挤回 pending。解析只保存
//! `Installed/AlreadyOwned` 结果，不持已验证的条目快照；整批结束后
//! `plan_required_with_resolution` 才按热表查找——被挤出的目标找不到
//! owner，`Installed` 的兜底把 miss 压成 `Missing`。
//!
//! 反例：hot cap=2 ＋ required A/B/C 三个合法可降级冷页＋模型预算足够——
//! A 因 C 的安装被降回 pending，规划报 Missing（红）。修复后解析在读取时
//! 捕获版本/范围绑定的条目（pending 行授权的那张卡），规划不再依赖整批
//! 结束时谁还恰好驻留；真实预算不足报 `BudgetExcluded`，不宣称 `Missing`。

use agent_contracts::{
    AnchorRootClaim, AnchorRootStrength, ContextConsumptionAck, ContextEngine, ContextHints,
    ContextItemId, ContextKind, ContextMaterializationMissReason, ContextQuery, ContextResidency,
    ContextRetention, ContextScope, OperationId, ResourceKey, RootReason, TurnId,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::open_focus;

/// 固定预算配置：全部冷条目卡片化（inline 目标 0）、restore 只读一个批次、
/// 热目录条数上限是本切片的被测对象（字节上限保持默认宽裕）。
fn b1_config(dir: &tempfile::TempDir, hot_entries: usize) -> SimpleContextConfig {
    SimpleContextConfig {
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 2,
        external_hydrate_max_items: 64,
        external_hot_metadata_max_entries: hot_entries,
        gc_buffer_capacity: 0,
        external_checkpoint_io_budget_ms: 60_000,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    }
}

/// Externalize one note (eviction buffer empty → straight to the store) and
/// pin its residency at External so the spill gate sees it. `source` 与
/// `entities` 直接落到条目上：`None` source＋首行路径正文构成文件体条目，
/// 实体键供 required 实体声明命中。
async fn externalize_stamped(
    engine: &SimpleContextEngine,
    body: &str,
    source: Option<String>,
    entities: Vec<String>,
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
            source,
        );
        item.scope_id = None;
        item.residency = ContextResidency::Warm;
        item.evicted_at_tick = Some(0);
        item.entities = entities;
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

/// The pending row for `id`, proving the entry is NOT resident (the id is
/// known only as a cold locator).
async fn pending_row_for(engine: &SimpleContextEngine, id: ContextItemId) -> bool {
    pending_row_hash(engine, id).await.is_some()
}

/// The pending row's card hash, for version-binding assertions.
async fn pending_row_hash(engine: &SimpleContextEngine, id: ContextItemId) -> Option<String> {
    let state = engine.state.lock().await;
    state
        .pending_external_cards
        .iter()
        .find(|(row_id, _)| *row_id == id)
        .map(|(_, hash)| hash.clone())
}

fn required_claim(item_ref: String) -> AnchorRootClaim {
    AnchorRootClaim {
        item_ref,
        strength: AnchorRootStrength::PromptRequired,
        source_field_id: "working_refs".into(),
        anchor_revision: 3,
        reason: RootReason::HardConstraint,
    }
}

/// Build the shared fixture: two fillers (restore's first batch) followed by
/// A/B/C, all cold. The restored engine holds exactly the fillers hot; every
/// target stays a pending row until the resolution reads it.
async fn abc_fixture(dir: &tempfile::TempDir) -> (SimpleContextEngine, [ContextItemId; 3]) {
    let source = SimpleContextEngine::new(b1_config(dir, 2));
    open_focus(&source, "batch required plan probe").await;
    let _f1 = externalize_stamped(&source, "payload-fill-one", None, Vec::new()).await;
    let _f2 = externalize_stamped(&source, "payload-fill-two", None, Vec::new()).await;
    let a = externalize_stamped(&source, "payload-req-alpha", None, Vec::new()).await;
    let b = externalize_stamped(&source, "payload-req-beta", None, Vec::new()).await;
    let c = externalize_stamped(&source, "payload-req-gamma", None, Vec::new()).await;
    let checkpoint = source.checkpoint().await.unwrap();

    let engine = SimpleContextEngine::new(b1_config(dir, 2));
    engine.restore(checkpoint).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.external.len(), 2, "setup: restore pages one batch");
        assert_eq!(state.pending_external_cards.len(), 3, "setup: A/B/C cold");
    }
    (engine, [a, b, c])
}

/// B1 核心反例（红-first）：hot cap=2 ＋ required A/B/C 三个合法可降级冷
/// 页＋模型预算足够 → 读取均成功且 A 不因随后驱逐成为 Missing。
///
/// 解析顺序 A→B→C：A、B 先后安装并互相把 filler 挤出；C 的安装把 A 挤回
/// pending。规划在整批结束后才查热表——旧代码 A 找不到 owner，`Installed`
/// 兜底成 Missing。
#[tokio::test]
async fn required_bodies_survive_demotion_caused_by_later_installs_in_the_same_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: ContextHints {
                anchor_roots: vec![
                    required_claim(format!("context://run/{a}")),
                    required_claim(format!("context://run/{b}")),
                    required_claim(format!("context://run/{c}")),
                ],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    // 被测对象确实发生了批内驱逐：A 在规划时不在热表（否则本测证明不了
    // fallback，只是 fixture 顺序巧合）。
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(a).is_none(),
            "setup: C's install must have demoted A back to pending"
        );
    }
    assert!(pending_row_for(&engine, a).await, "A keeps its cold row");

    assert!(
        materialized.required_misses.is_empty(),
        "every required body was read successfully this operation; nothing may miss: {:?}",
        materialized.required_misses
    );
    for (id, sentinel) in [
        (a, "payload-req-alpha"),
        (b, "payload-req-beta"),
        (c, "payload-req-gamma"),
    ] {
        let served = materialized
            .items
            .iter()
            .find(|item| item.item_id == id)
            .unwrap_or_else(|| panic!("required body {id} must reach the frame"));
        assert!(
            served.content.contains(sentinel),
            "the served body is the captured one: {}",
            served.content
        );
    }
}

/// 真实预算不足：同一 fixture、模型预算 1——放不下的必需正文报准确的
/// `BudgetExcluded`，不因批内驱逐压成 `Missing`（旧代码在 A 上先红）。
#[tokio::test]
async fn required_over_demoted_cold_reads_report_budget_excluded_not_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 1,
            hints: ContextHints {
                anchor_roots: vec![
                    required_claim(format!("context://run/{a}")),
                    required_claim(format!("context://run/{b}")),
                    required_claim(format!("context://run/{c}")),
                ],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let misses = materialized.required_misses.as_slice();
    assert_eq!(
        misses.len(),
        3,
        "every required ref reports exactly one miss: {misses:?}"
    );
    for miss in misses {
        assert_eq!(
            miss.reason,
            ContextMaterializationMissReason::BudgetExcluded,
            "a body that was read this operation names the budget, never absence: {misses:?}"
        );
    }
}

/// 混合 exact ID＋实体＋前景路径：批内驱逐同时打击三类声明。冷行顺序
/// [F1, A, B, C, F2]（cap=2）逐卡安装互相驱逐：exact A 被 B 挤出、实体卡
/// B 被 F2 挤出、前景 F1 被 C 挤出；C、F2 驻留。解析捕获让被驱逐的目标按
/// pending 行授权的版本进入计划，而不是丢给 Missing／UnreadColdPage。
/// （前景条数恰为合约上限 `MAX_FOREGROUND_RESOURCES=2`，不触预算排除。）
#[tokio::test]
async fn mixed_exact_entity_and_path_refs_survive_batch_demotion() {
    let dir = tempfile::tempdir().unwrap();
    let source = SimpleContextEngine::new(b1_config(&dir, 2));
    open_focus(&source, "mixed refs probe").await;
    let _f1 = externalize_stamped(&source, "payload-fill-one", None, Vec::new()).await;
    let _f2 = externalize_stamped(&source, "payload-fill-two", None, Vec::new()).await;
    // 文件体条目：source 为空＋首行路径正文（`primary_file_path`/
    // `is_file_body_entry` 口径），路径同时盖章进实体——与真实管线的
    // `index_file_path` 同形，冷目录扫描按实体命中路径。
    let fg_one = externalize_stamped(
        &source,
        "cold-fg-one.rs\npayload-fg-one-body",
        None,
        vec!["cold-fg-one.rs".into()],
    )
    .await;
    let a = externalize_stamped(&source, "payload-req-alpha", None, Vec::new()).await;
    let b = externalize_stamped(
        &source,
        "payload-req-beta-one",
        None,
        vec!["shared-required-entity".into()],
    )
    .await;
    let c = externalize_stamped(
        &source,
        "payload-req-beta-two",
        None,
        vec!["shared-required-entity".into()],
    )
    .await;
    let fg_two = externalize_stamped(
        &source,
        "cold-fg-two.rs\npayload-fg-two-body",
        None,
        vec!["cold-fg-two.rs".into()],
    )
    .await;
    let checkpoint = source.checkpoint().await.unwrap();

    let engine = SimpleContextEngine::new(b1_config(&dir, 2));
    engine.restore(checkpoint).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(state.external.len(), 2, "setup: restore pages one batch");
        assert_eq!(
            state.pending_external_cards.len(),
            5,
            "setup: five cold rows"
        );
    }

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: ContextHints {
                anchor_roots: vec![
                    required_claim(format!("context://run/{a}")),
                    required_claim("shared-required-entity".into()),
                ],
                foreground_resources: vec![
                    ResourceKey {
                        path: "cold-fg-one.rs".into(),
                        revision: None,
                    },
                    ResourceKey {
                        path: "cold-fg-two.rs".into(),
                        revision: None,
                    },
                ],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    // 批内驱逐确实发生：A、F1、B 都被后续安装挤出热表（规划时的真实状态，
    // 不是 fixture 巧合）；C、F2 还驻留。
    {
        let state = engine.state.lock().await;
        for (name, id) in [("A", a), ("B", b), ("F1", fg_one)] {
            assert!(
                state.external.get(id).is_none(),
                "setup: {name} must have been demoted by a later install in the same batch"
            );
        }
        assert!(state.external.get(c).is_some(), "setup: C stays hot");
        assert!(state.external.get(fg_two).is_some(), "setup: F2 stays hot");
    }

    let miss_refs = |misses: &[agent_contracts::ContextMaterializationMiss]| {
        misses
            .iter()
            .map(|miss| miss.identity.item_ref.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        materialized.required_misses.is_empty(),
        "exact A and the entity ref (B/C) were all read this operation: {:?}",
        miss_refs(materialized.required_misses.as_slice())
    );
    for (id, sentinel) in [
        (a, "payload-req-alpha"),
        (b, "payload-req-beta-one"),
        (c, "payload-req-beta-two"),
    ] {
        let served = materialized
            .items
            .iter()
            .find(|item| item.item_id == id)
            .unwrap_or_else(|| panic!("required body {id} must reach the frame"));
        assert!(
            served.content.contains(sentinel),
            "the served body is the captured one: {}",
            served.content
        );
    }

    assert!(
        materialized.optional_misses.is_empty(),
        "both foreground paths were read this operation: {:?}",
        miss_refs(materialized.optional_misses.as_slice())
    );
    for (id, sentinel) in [
        (fg_one, "payload-fg-one-body"),
        (fg_two, "payload-fg-two-body"),
    ] {
        let served = materialized
            .foreground
            .iter()
            .find(|item| item.item_id == id)
            .unwrap_or_else(|| panic!("foreground body {id} must reach the frame"));
        assert!(
            served.content.contains(sentinel),
            "the served body is the captured one: {}",
            served.content
        );
    }
}

/// 切片 A（2026-09-16 review 6afa25df Q1，红先）：材料化把 A/B/C 三份正文送
/// 进最终帧（A 随后被 C 的安装挤回 pending 冷驻留）之后，Runtime 会把最终
/// `materialized.items` 的全部 id 放进真实 `ContextConsumptionAck`
/// （model.rs 最终帧 ACK 直接来自 items）。旧 owner 计数只查 heap /
/// eviction_buffer / pending_externalize_retry / 已加载 external 四处——A
/// 四处皆空，ACK 被拒，本轮成功结果无法提交。红即 unwrap。
#[tokio::test]
async fn consumption_ack_accepts_the_pending_cold_body_the_preview_served() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 100_000,
            hints: ContextHints {
                anchor_roots: vec![
                    required_claim(format!("context://run/{a}")),
                    required_claim(format!("context://run/{b}")),
                    required_claim(format!("context://run/{c}")),
                ],
                ..Default::default()
            },
        })
        .await
        .unwrap();

    // 保留 B1 的既有断言：A 确实 pending 冷驻留——本测证明的不是热驻留。
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(a).is_none(),
            "setup: C's install must have demoted A back to pending"
        );
    }
    assert!(pending_row_for(&engine, a).await, "A keeps its cold row");

    // 真实 Runtime 形状：最终帧的全部 id 进 ACK（不只是仍驻留的子集）。
    engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: materialized.materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
            foreground_item_ids: materialized
                .foreground
                .iter()
                .map(|item| item.item_id)
                .collect(),
        })
        .await
        .unwrap();

    // 消费结算的诚实形状：三个 id 各记一次聚合 ack；A（冷）的逐项事实以
    // `(id, 卡片哈希, turn, tick)` 记录存续，B/C（驻留）直接盖戳。
    // 卡片哈希先在锁外读：tokio Mutex 不可重入，持锁跨 .await 再锁同一把
    // 锁会在 current_thread runtime 上永久死锁。
    let row_hash = pending_row_hash(&engine, a)
        .await
        .expect("A's row survives");
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.access_consumption_acks, 3,
            "each acked id settles exactly one consumption fact"
        );
        assert_eq!(
            state.pending_cold_consumed.len(),
            1,
            "exactly the demoted id needs the persisted cold record: {:?}",
            state.pending_cold_consumed
        );
        let record = &state.pending_cold_consumed[0];
        assert_eq!(record.item_id, a, "the cold record is A's");
        assert_eq!(
            record.card_hash, row_hash,
            "the record is bound to the exact card version the preview served"
        );
    }
    // checkpoint/restore 一致：持久化记录随状态走，恢复后仍能落账。
    let checkpoint = engine.checkpoint().await.unwrap();
    let restored = SimpleContextEngine::new(b1_config(&dir, 2));
    restored.restore(checkpoint).await.unwrap();

    // A 的卡片按 id 水化：记录把真实的消费事实盖到新驻留条目上（延迟
    // 记账，不是伪造强化——没有额外计数之外的强化，GC 世代锚不动）。
    let fetched = restored
        .fetch_external(a)
        .await
        .unwrap()
        .expect("the consumed version still fetches by id");
    assert!(
        fetched.content.contains("payload-req-alpha"),
        "the fetched body is the served one: {}",
        fetched.content
    );
    {
        let state = restored.state.lock().await;
        let entry = state.external.get(a).expect("the card installed");
        assert_eq!(
            entry.access_count, 1,
            "the one true consumption lands, nothing more"
        );
        assert_eq!(
            entry.last_access_turn, state.turn,
            "the landing records the turn the model actually consumed, not the hydration turn"
        );
        assert!(
            state.pending_cold_consumed.is_empty(),
            "the record left the ring once it landed: {:?}",
            state.pending_cold_consumed
        );
    }
}

/// 旧拒绝语义保持：错误 materialization id 的 ACK 仍被拒绝——预览匹配
/// 先于 owner 校验，冷修复不放宽它。
#[tokio::test]
async fn ack_with_a_wrong_materialization_id_is_still_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(required_abc_query(a, b, c))
        .await
        .unwrap();

    let error = engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: materialized.materialization_id.wrapping_add(1),
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: Vec::new(),
            foreground_item_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("is pending"),
        "the stale-preview rejection names the id mismatch: {error}"
    );
}

/// 旧拒绝语义保持：预览之外的外来 id 仍被拒绝。
#[tokio::test]
async fn ack_with_a_foreign_id_is_still_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(required_abc_query(a, b, c))
        .await
        .unwrap();

    let error = engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: materialized.materialization_id,
            item_ids: materialized
                .items
                .iter()
                .map(|item| item.item_id)
                .chain(std::iter::once(ContextItemId::new()))
                .collect(),
            external_item_ids: Vec::new(),
            foreground_item_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("outside the referenced preview"),
        "a foreign id never passes the preview containment check: {error}"
    );
}

/// 版本绑定：预览送出 A 的卡片版本后，若 pending 定位行的哈希不再是预览
/// 时那张（元数据变更后重写卡），ACK 必须拒绝——「同 ID 出现过」不是
/// owner，被送出的版本才算。
#[tokio::test]
async fn ack_over_a_resolved_cold_id_with_a_changed_card_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [a, b, c]) = abc_fixture(&dir).await;

    let materialized = engine
        .materialize(required_abc_query(a, b, c))
        .await
        .unwrap();
    assert!(pending_row_for(&engine, a).await, "A keeps its cold row");

    // A 的卡片版本在预览之后被替换（模拟重写卡）：预览捕获的哈希失效。
    {
        let mut state = engine.state.lock().await;
        for (row_id, hash) in state.pending_external_cards.iter_mut() {
            if *row_id == a {
                *hash = "deadbeef0000".into();
            }
        }
    }

    let error = engine
        .acknowledge_consumption(ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 1,
            materialization_id: materialized.materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: Vec::new(),
            foreground_item_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("without exactly one residency owner"),
        "the served card version no longer has an owner, so the ack rejects: {error}"
    );
}

/// 共享 query：required 声明 A/B/C 三份正文（模型预算足够）。
fn required_abc_query(a: ContextItemId, b: ContextItemId, c: ContextItemId) -> ContextQuery {
    ContextQuery {
        current_input: "continue".into(),
        budget_tokens: 100_000,
        hints: ContextHints {
            anchor_roots: vec![
                required_claim(format!("context://run/{a}")),
                required_claim(format!("context://run/{b}")),
                required_claim(format!("context://run/{c}")),
            ],
            ..Default::default()
        },
    }
}

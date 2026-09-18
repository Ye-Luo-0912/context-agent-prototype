//! CTX-11 (H2)：语义生命周期与正文位置的等价——五位置同一结局。
//!
//! 审查基线（2026-09-18）：`queue_decision_supersessions` /
//! `queue_error_verifications` 扫描四个已加载位置，唯独 F2 spill 卡片的
//! 未加载 `(id, card hash)` 行不在扫描内。同一条「Remove X」对驻留旧决策
//! 是明确撤销，对降到未加载冷卡片的同一条决策却扫描不到；卡片随后水化时
//! 按旧语义状态装回，Live 复活。
//!
//! 本模块钉住：
//! - 五位置等价（heap / warm buffer / externalize-retry / 已加载 external /
//!   未加载冷卡片）对同一条明确「Remove X」产出同一终态；
//! - 意图环随 State 持久化：checkpoint→restore 不丢意图，也不复活旧 Live；
//! - 跨任务同实体不误终结，意图保留；
//! - 卡读取瞬时失败（IoFailed）意图保留、可重试；
//! - 冷卡片上的 Error 由同任务同配方成功探针在安装时 VerifiedFixed。
//!
//! CTX-11 (B-1，审查 BR1)：冷页语义更新义务的**目标集合与因果边界**——
//! - 同一意图的多条目标分批安装逐条结算，全部已知名额处理完才 consumed；
//! - 意图绑定记录时点的因果上界，后来新建的决策永不为旧意图的目标；
//! - 否定/保留结论在记录点用完整输入判定一次，截断副本只作诊断；
//! - Verify 形状匹配 ≠ 结算：证据不可读时 Unresolved（保留、不消费、
//!   不落终态），证据可读后正常结算；
//! - 结算/候选记账有界，溢出有诚实计数。

use std::sync::atomic::Ordering;

use agent_contracts::{
    ContextEngine, ContextIngress, ContextItemId, ContextKind, ContextMaintenanceTrigger,
    ContextResidency, SemanticState,
};

use crate::engine::{PendingIdOutcome, SimpleContextConfig, SimpleContextEngine};

use super::harness::{open_focus, verify_failure_observation, verify_observation};

const OLD: &str = "use AuthService.rs with a 5-second timeout";
const REMOVE: &str = "Remove AuthService.rs";

/// 未加载冷卡片形状：inline 目标 0（一切外置条目都进卡片）、restore 批次 0
/// （restore 一行都不读入，全部留在 pending 目录）。
fn cold_config(dir: &tempfile::TempDir) -> SimpleContextConfig {
    SimpleContextConfig {
        context_store_dir: Some(dir.path().to_path_buf()),
        external_checkpoint_inline_target: 0,
        external_restore_card_batch: 0,
        external_checkpoint_io_budget_ms: 60_000,
        ..SimpleContextConfig::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Heap,
    WarmBuffer,
    ExternalizeRetry,
    LoadedExternal,
    PendingColdCard,
}

/// 把一条真实 ingest 的旧决策放进指定正文位置，返回（引擎, 旧决策 id, 任务）。
/// 未加载冷卡片位置走真实 capture→restore（批次 0），其余位置直接搬运。
async fn engine_with_old_decision_at(
    dir: &tempfile::TempDir,
    position: Position,
) -> (SimpleContextEngine, ContextItemId, agent_contracts::TaskId) {
    let first = SimpleContextEngine::new(cold_config(dir));
    let task = open_focus(&first, "five positions").await;
    first
        .ingest(ContextIngress::UserMessage {
            content: OLD.into(),
        })
        .await
        .unwrap();
    if position == Position::Heap {
        let old_id = heap_decision_id(&first).await;
        return (first, old_id, task);
    }
    let item = {
        let mut state = first.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.content == OLD)
            .expect("the old decision item exists")
            .clone();
        let kept: Vec<_> = state
            .items
            .iter()
            .filter(|row| row.id != item.id)
            .cloned()
            .collect();
        state.items.replace_all(kept);
        item
    };
    let old_id = item.id;
    {
        let mut state = first.state.lock().await;
        match position {
            Position::WarmBuffer => state.eviction_buffer.push(item.clone()),
            Position::ExternalizeRetry => state.pending_externalize_retry.push(item.clone()),
            // 已加载 external 与未加载冷卡片共用同一条真实外置路径；
            // 冷卡片位置随后 capture→restore（批次 0）把它降回 pending 行。
            Position::LoadedExternal | Position::PendingColdCard => {
                let reference = crate::store::externalize(dir.path(), &item).unwrap();
                state.external.push(crate::store::to_external_entry(
                    &item, reference, 1, 1, None,
                ));
            }
            Position::Heap => unreachable!("returned above"),
        }
        state.sync_catalog();
    }
    if position != Position::PendingColdCard {
        return (first, old_id, task);
    }
    // PendingColdCard：真实外置（residency External 让 spill gate 看到）→
    // capture 写卡 → 批次 0 restore 让该行留在 pending 目录。
    {
        let mut state = first.state.lock().await;
        let entry = state.external.get_mut(old_id).expect("externalized");
        entry.residency = ContextResidency::External;
        state.sync_catalog();
    }
    let value = first.checkpoint().await.unwrap();
    let spilled = value
        .get("external_spilled")
        .and_then(|v| v.as_array())
        .expect("inline target 0 spills the decision")
        .len();
    assert_eq!(spilled, 1, "the old decision is one manifest row");
    let engine = SimpleContextEngine::new(cold_config(dir));
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.external.get(old_id).is_none(),
            "restore batch 0 keeps the card unloaded"
        );
        assert!(
            state
                .pending_external_cards
                .iter()
                .any(|(id, _)| *id == old_id),
            "the old decision is a pending cold row"
        );
    }
    (engine, old_id, task)
}

async fn heap_decision_id(engine: &SimpleContextEngine) -> ContextItemId {
    engine
        .state
        .lock()
        .await
        .items
        .iter()
        .find(|item| item.content == OLD)
        .expect("the old decision item exists")
        .id
}

/// 语义状态只看事实，不看正文当前坐在哪个位置。
fn semantic_at(state: &crate::engine::State, id: ContextItemId) -> Option<SemanticState> {
    if let Some(item) = state.items.iter().find(|item| item.id == id) {
        return Some(item.semantic);
    }
    if let Some(item) = state.eviction_buffer.iter().find(|item| item.id == id) {
        return Some(item.semantic);
    }
    if let Some(item) = state
        .pending_externalize_retry
        .iter()
        .find(|item| item.id == id)
    {
        return Some(item.semantic);
    }
    state.external.get(id).map(|entry| entry.semantic)
}

/// H2 核心（红例先行）：同一条旧 Decision 分别坐在五个正文位置，收到同一
/// 条明确「Remove X」，语义结局必须等价——Superseded{by 新决策}。前四个
/// 位置走已加载扫描＋维护排空，第五个走安装路径（意图在 ingest 记录，
/// fetch 安装时应用）。
#[tokio::test]
async fn explicit_removal_finalizes_the_old_decision_in_all_five_body_locations() {
    for position in [
        Position::Heap,
        Position::WarmBuffer,
        Position::ExternalizeRetry,
        Position::LoadedExternal,
        Position::PendingColdCard,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (engine, old_id, _task) = engine_with_old_decision_at(&dir, position).await;
        engine
            .ingest(ContextIngress::UserMessage {
                content: REMOVE.into(),
            })
            .await
            .unwrap();
        if position == Position::PendingColdCard {
            {
                let state = engine.state.lock().await;
                assert_eq!(
                    state.pending_cold_semantic_intents.len(),
                    1,
                    "{position:?}: the removal is recorded against the pending cold card"
                );
            }
            // 安装路径应用意图（这里走 per-id 服务；批量排空共享同一函数）。
            // 断言用类型化安装结果而不是 fetch 的返回体：终态条目本来就不再
            // 可检索——安装成功＋语义终态才是本测试的事实。
            let read = engine.hydrate_card_for_outcome(old_id).await;
            assert_eq!(
                read.outcome,
                PendingIdOutcome::Installed,
                "{position:?}: the cold card installs"
            );
        }
        let report = engine
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await
            .unwrap();
        let state = engine.state.lock().await;
        let new_id = state
            .items
            .iter()
            .find(|item| item.content == REMOVE)
            .expect("the removing decision exists")
            .id;
        assert_eq!(
            semantic_at(&state, old_id),
            Some(SemanticState::Superseded { by: Some(new_id) }),
            "{position:?}: all five body locations must reach the same terminal outcome"
        );
        assert!(
            report
                .transitions
                .iter()
                .any(|t| t.item_id == old_id && t.reason.contains("superseded by decision")),
            "{position:?}: the terminal change is observable in the maintenance report: {:?}",
            report.transitions
        );
        assert!(
            state.pending_cold_semantic_intents.is_empty(),
            "{position:?}: a matched intent is consumed"
        );
    }
}

/// H2 持久化：意图随 State 序列化。Remove X 记录后 checkpoint→restore，
/// 意图仍在、冷卡仍未加载；随后安装照常应用。再一轮 capture→restore→
/// fetch（此时卡片已按终态重序列化）不得把旧 Live 复活。
#[tokio::test]
async fn a_deferred_cold_supersession_survives_restore_and_never_revives_old_live() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, old_id, _task) =
        engine_with_old_decision_at(&dir, Position::PendingColdCard).await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: REMOVE.into(),
        })
        .await
        .unwrap();
    assert_eq!(
        engine
            .state
            .lock()
            .await
            .pending_cold_semantic_intents
            .len(),
        1,
        "the intent is recorded"
    );
    // checkpoint→restore：意图不丢，冷卡保持未加载。
    let value = engine.checkpoint().await.unwrap();
    let engine = SimpleContextEngine::new(cold_config(&dir));
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "restore keeps the deferred intent"
        );
        assert!(
            state.external.get(old_id).is_none(),
            "restore leaves the card unloaded"
        );
    }
    // 安装：意图应用，旧决策终态。
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::Installed
    );
    let new_id = {
        let state = engine.state.lock().await;
        let new_id = state
            .items
            .iter()
            .find(|item| item.content == REMOVE)
            .expect("the removing decision exists")
            .id;
        assert_eq!(
            semantic_at(&state, old_id),
            Some(SemanticState::Superseded { by: Some(new_id) })
        );
        assert!(state.pending_cold_semantic_intents.is_empty());
        new_id
    };
    // 第二轮 capture→restore→fetch：卡片已按终态重序列化，旧 Live 不复活。
    let value = engine.checkpoint().await.unwrap();
    let engine = SimpleContextEngine::new(cold_config(&dir));
    engine.restore(value).await.unwrap();
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, old_id),
        Some(SemanticState::Superseded { by: Some(new_id) }),
        "checkpoint→restore→fetch must not resurrect the old Live decision"
    );
}

/// H2 身份规则：跨任务同实体不误终结。task B 的「Remove X」对 task A 的
/// 冷卡片旧决策既不应用、也不消费——意图保留等真正的安装证据。
#[tokio::test]
async fn a_cross_task_cold_removal_keeps_the_old_decision_live_and_the_intent_retained() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, old_id, _task_a) =
        engine_with_old_decision_at(&dir, Position::PendingColdCard).await;
    let _task_b = open_focus(&engine, "a different task").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: REMOVE.into(),
        })
        .await
        .unwrap();
    assert_eq!(
        engine
            .state
            .lock()
            .await
            .pending_cold_semantic_intents
            .len(),
        1,
        "the removal is recorded (the gate is task-agnostic; identity decides at apply time)"
    );
    // 安装发生（意图身份不匹配 → 不应用）：语义仍是 Live，意图保留。
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, old_id),
        Some(SemanticState::Live),
        "another task's removal must not finalize this task's decision"
    );
    assert_eq!(
        state.pending_cold_semantic_intents.len(),
        1,
        "an unmatched intent stays queued for a later install"
    );
}

/// H2 诚实语义：卡片读取瞬时失败不是「数据不存在」。IoFailed 不安装、
/// 不消费意图、locator 保留；下一次成功读取照常应用意图。
#[tokio::test]
async fn a_transient_card_read_failure_keeps_the_deferred_intent_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, old_id, _task) =
        engine_with_old_decision_at(&dir, Position::PendingColdCard).await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: REMOVE.into(),
        })
        .await
        .unwrap();
    engine.card_read_failure_bomb.store(1, Ordering::Relaxed);
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::IoFailed,
        "a transient read failure is not an install"
    );
    {
        let state = engine.state.lock().await;
        assert!(state.external.get(old_id).is_none());
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "the intent is not consumed by a failed install"
        );
        assert_eq!(
            state.pending_external_cards.len(),
            1,
            "the locator stays retryable"
        );
    }
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::Installed,
        "the retried install succeeds"
    );
    let state = engine.state.lock().await;
    let new_id = state
        .items
        .iter()
        .find(|item| item.content == REMOVE)
        .expect("the removing decision exists")
        .id;
    assert_eq!(
        semantic_at(&state, old_id),
        Some(SemanticState::Superseded { by: Some(new_id) }),
        "the retried install applies the retained intent"
    );
    assert!(state.pending_cold_semantic_intents.is_empty());
}

/// H2 Verify 路径：冷卡片上的 Error 由同任务、同配方的成功探针在安装时
/// VerifiedFixed——与已加载位置的 queue_error_verifications 同一身份规则
/// （同一 probe 再经 has_matching_verification_evidence 复核）。
#[tokio::test]
async fn a_cold_card_error_is_verified_fixed_by_a_matching_probe_after_install() {
    let dir = tempfile::tempdir().unwrap();
    let first = SimpleContextEngine::new(cold_config(&dir));
    let _task = open_focus(&first, "cold verification").await;
    verify_failure_observation(
        &first,
        "failed",
        "failure in AuthService.rs:42",
        "auth.tests",
    )
    .await;
    let error_id = {
        let mut state = first.state.lock().await;
        let item = state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("the recorded error exists")
            .clone();
        let kept: Vec<_> = state
            .items
            .iter()
            .filter(|row| row.id != item.id)
            .cloned()
            .collect();
        state.items.replace_all(kept);
        let reference = crate::store::externalize(dir.path(), &item).unwrap();
        let mut entry = crate::store::to_external_entry(&item, reference, 1, 1, None);
        entry.residency = ContextResidency::External;
        state.external.push(entry);
        state.sync_catalog();
        item.id
    };
    let value = first.checkpoint().await.unwrap();
    let engine = SimpleContextEngine::new(cold_config(&dir));
    engine.restore(value).await.unwrap();
    assert!(
        engine
            .state
            .lock()
            .await
            .pending_external_cards
            .iter()
            .any(|(id, _)| *id == error_id),
        "setup: the error waits on a pending cold card"
    );
    // 焦点随 checkpoint 恢复（同一任务）；同配方成功 → 记 Verify 意图。
    verify_observation(&engine, "pass", "fixed").await;
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "the matching trusted success records a deferred verification"
        );
    }
    assert_eq!(
        engine.hydrate_card_for_outcome(error_id).await.outcome,
        PendingIdOutcome::Installed,
        "the cold card installs"
    );
    let state = engine.state.lock().await;
    let by_id = state
        .items
        .iter()
        .find(|item| item.kind == ContextKind::ToolObservation)
        .expect("the verifying observation exists")
        .id;
    assert_eq!(
        semantic_at(&state, error_id),
        Some(SemanticState::VerifiedFixed { by: Some(by_id) }),
        "the cold error is finalized exactly like a loaded one"
    );
    assert!(state.pending_cold_semantic_intents.is_empty());
}

// ---------------------------------------------------------------------------
// CTX-11 (B-1，审查 BR1)：冷页语义更新义务的目标集合与因果边界。
// ---------------------------------------------------------------------------

/// 把堆上第一条命中谓词的条目外置为 External 元数据条目（下一个 capture
/// 会把它写成卡片行），返回其 id。
async fn externalize_heap_item<F>(
    engine: &SimpleContextEngine,
    dir: &tempfile::TempDir,
    matches: F,
) -> ContextItemId
where
    F: Fn(&agent_contracts::ContextItem) -> bool,
{
    let mut state = engine.state.lock().await;
    let item = state
        .items
        .iter()
        .find(|item| matches(item))
        .expect("the heap item exists")
        .clone();
    let kept: Vec<_> = state
        .items
        .iter()
        .filter(|row| row.id != item.id)
        .cloned()
        .collect();
    state.items.replace_all(kept);
    let reference = crate::store::externalize(dir.path(), &item).unwrap();
    let mut entry = crate::store::to_external_entry(&item, reference, 1, 1, None);
    entry.residency = ContextResidency::External;
    state.external.push(entry);
    state.sync_catalog();
    item.id
}

/// 把堆上所有命中谓词的条目从堆里移除（隔离已加载扫描等干扰，测试对象
/// 只剩冷意图路径本身）。
async fn remove_heap_items<F>(engine: &SimpleContextEngine, matches: F)
where
    F: Fn(&agent_contracts::ContextItem) -> bool,
{
    let mut state = engine.state.lock().await;
    let kept: Vec<_> = state
        .items
        .iter()
        .filter(|row| !matches(row))
        .cloned()
        .collect();
    state.items.replace_all(kept);
    state.sync_catalog();
}

/// capture（inline target 0：全部外置条目写卡）→ 批次 0 restore：所有卡片
/// 行留在 pending 目录，堆内容随 checkpoint 原样保留。
async fn recapture_with_pending_cards(
    engine: &SimpleContextEngine,
    dir: &tempfile::TempDir,
) -> SimpleContextEngine {
    let value = engine.checkpoint().await.unwrap();
    let next = SimpleContextEngine::new(cold_config(dir));
    next.restore(value).await.unwrap();
    let state = next.state.lock().await;
    assert!(
        !state.pending_external_cards.is_empty(),
        "setup: inline target 0 leaves every card row pending"
    );
    drop(state);
    next
}

/// 意图是 serde 外部 tagged 枚举（`{"Supersede": {..}}` / `{"Verify":
/// {..}}`），取 checkpoint 里第 `index` 条意图的负载对象。
fn intent_payload(value: &serde_json::Value, index: usize) -> &serde_json::Value {
    let intent = &value["pending_cold_semantic_intents"][index];
    intent
        .get("Supersede")
        .or_else(|| intent.get("Verify"))
        .expect("the intent payload object")
}

/// [`intent_payload`] 的可变版本（测试直接改写账本字段构造溢出场景）。
fn intent_payload_mut(value: &mut serde_json::Value, index: usize) -> &mut serde_json::Value {
    let intent = &mut value["pending_cold_semantic_intents"][index];
    if intent.get("Supersede").is_some() {
        &mut intent["Supersede"]
    } else {
        &mut intent["Verify"]
    }
}

/// 两条同内容旧决策 A、B 都是未加载冷卡片，同一条 Remove 已记录意图，
/// 返回（引擎, [id_a, id_b], 新决策 id）。
async fn engine_with_two_pending_old_decisions(
    dir: &tempfile::TempDir,
) -> (SimpleContextEngine, [ContextItemId; 2], ContextItemId) {
    let first = SimpleContextEngine::new(cold_config(dir));
    let _task = open_focus(&first, "two cold targets").await;
    first
        .ingest(ContextIngress::UserMessage {
            content: OLD.into(),
        })
        .await
        .unwrap();
    first
        .ingest(ContextIngress::UserMessage {
            content: OLD.into(),
        })
        .await
        .unwrap();
    let id_a = externalize_heap_item(&first, dir, |item| item.content == OLD).await;
    let id_b = externalize_heap_item(&first, dir, |item| item.content == OLD).await;
    assert_ne!(id_a, id_b, "setup: two distinct old decisions");
    let engine = recapture_with_pending_cards(&first, dir).await;
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_external_cards.len(),
            2,
            "setup: both old decisions wait on pending cards"
        );
    }
    engine
        .ingest(ContextIngress::UserMessage {
            content: REMOVE.into(),
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    assert_eq!(
        state.pending_cold_semantic_intents.len(),
        1,
        "setup: the removal is recorded once against both pending targets"
    );
    let new_id = state
        .items
        .iter()
        .find(|item| item.content == REMOVE)
        .expect("the removing decision exists")
        .id;
    drop(state);
    (engine, [id_a, id_b], new_id)
}

/// B-1 反例 1：同一撤销意图匹配的两条旧决策分两批安装。安装时任意命中即
/// 整条消费，会让第二批的旧决策保持 Live。意图必须逐目标结算：处理 A 不清
/// B，全部已知名额处理完才 consumed。
#[tokio::test]
async fn one_removal_intent_settles_two_cold_decisions_across_two_installs() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [id_a, id_b], new_id) = engine_with_two_pending_old_decisions(&dir).await;
    // 第一批：只安装 A。结算 A，意图必须保留等 B。
    assert_eq!(
        engine.hydrate_card_for_outcome(id_a).await.outcome,
        PendingIdOutcome::Installed
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            semantic_at(&state, id_a),
            Some(SemanticState::Superseded { by: Some(new_id) }),
            "the first target settles on its install"
        );
        assert!(
            state
                .pending_external_cards
                .iter()
                .any(|(id, _)| *id == id_b),
            "the second target is still an unloaded obligation, not dropped"
        );
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "the intent stays open until every record-time target is resolved"
        );
    }
    // 第二批：安装 B，意图仍存在并命中。
    assert_eq!(
        engine.hydrate_card_for_outcome(id_b).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, id_b),
        Some(SemanticState::Superseded { by: Some(new_id) }),
        "the retained intent still applies to the second target"
    );
    assert!(
        state.pending_cold_semantic_intents.is_empty(),
        "the obligation completes only after the last known target settles"
    );
}

/// B-1 反例 2：意图绑定记录时点的因果上界。意图记录之后、同任务后来新建
/// 的同条件决策安装时，不得被这条更早的撤销终结——后来明确提出的要求不被
/// 更早的撤销作废。
#[tokio::test]
async fn an_intent_never_finalizes_a_decision_created_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let first = SimpleContextEngine::new(cold_config(&dir));
    let _task = open_focus(&first, "causal bound").await;
    first
        .ingest(ContextIngress::UserMessage {
            content: OLD.into(),
        })
        .await
        .unwrap();
    let old_id = externalize_heap_item(&first, &dir, |item| item.content == OLD).await;
    let engine = recapture_with_pending_cards(&first, &dir).await;
    // 意图记录：候选快照只含记录时点已 pending 的 old_id。
    engine
        .ingest(ContextIngress::UserMessage {
            content: REMOVE.into(),
        })
        .await
        .unwrap();
    let new_id = {
        let state = engine.state.lock().await;
        assert_eq!(state.pending_cold_semantic_intents.len(), 1);
        state
            .items
            .iter()
            .find(|item| item.content == REMOVE)
            .expect("the removing decision exists")
            .id
    };
    // 把 Remove 决策本身从堆里拿走，隔离已加载扫描——本测试的对象是冷意图。
    remove_heap_items(&engine, |item| item.content == REMOVE).await;
    // 同任务后来新建同条件决策（在意图之后创建）。
    engine
        .ingest(ContextIngress::UserMessage {
            content: OLD.into(),
        })
        .await
        .unwrap();
    let later_id = externalize_heap_item(&engine, &dir, |item| item.content == OLD).await;
    assert_ne!(later_id, old_id, "setup: the later decision is a new item");
    let engine = recapture_with_pending_cards(&engine, &dir).await;
    // 安装后建的这条：旧意图不得终结它。
    assert_eq!(
        engine.hydrate_card_for_outcome(later_id).await.outcome,
        PendingIdOutcome::Installed
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            semantic_at(&state, later_id),
            Some(SemanticState::Live),
            "a decision created after the intent is never its target"
        );
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "the intent is intact: the later decision was never part of its obligation"
        );
    }
    // 意图完好且对其真正的目标依然生效：原 pending 目标安装即被终结。
    assert_eq!(
        engine.hydrate_card_for_outcome(old_id).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, old_id),
        Some(SemanticState::Superseded { by: Some(new_id) }),
        "the causal bound protects the later decision, not the real target"
    );
    assert!(
        state.pending_cold_semantic_intents.is_empty(),
        "the whole record-time target universe resolved: the obligation completes"
    );
}

/// B-1 反例 3：Verify 三态。by 证据暂不可读时形状匹配不结算、不消费、
/// 不落终态（Unresolved）；证据可读后经再次安装正常结算。
#[tokio::test]
async fn an_unreadable_by_evidence_keeps_the_verify_intent_unresolved_until_readable() {
    let dir = tempfile::tempdir().unwrap();
    let first = SimpleContextEngine::new(cold_config(&dir));
    let _task = open_focus(&first, "cold verification").await;
    verify_failure_observation(
        &first,
        "failed",
        "failure in AuthService.rs:42",
        "auth.tests",
    )
    .await;
    let error_id =
        externalize_heap_item(&first, &dir, |item| item.kind == ContextKind::Error).await;
    let engine = recapture_with_pending_cards(&first, &dir).await;
    verify_observation(&engine, "pass", "fixed").await;
    let by_item = {
        let state = engine.state.lock().await;
        assert_eq!(state.pending_cold_semantic_intents.len(), 1);
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("the verifying observation exists")
            .clone()
    };
    // 证据暂不可读：by 观察不在任何可读位置（模拟已被丢弃的临时观察）。
    remove_heap_items(&engine, |item| item.kind == ContextKind::ToolObservation).await;
    assert_eq!(
        engine.hydrate_card_for_outcome(error_id).await.outcome,
        PendingIdOutcome::Installed
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            semantic_at(&state, error_id),
            Some(SemanticState::Live),
            "shape matching without readable evidence must not finalize the error"
        );
        assert!(
            state.pending_ingest_transitions.is_empty(),
            "no terminal transition without evidence"
        );
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "the obligation is NOT consumed: shape matching and settlement are different facts"
        );
    }
    // 证据恢复可读 → checkpoint→restore 让错误卡片重新走安装路径 → 结算。
    {
        let mut state = engine.state.lock().await;
        state.items.push(by_item.clone());
        state.sync_catalog();
    }
    let engine = recapture_with_pending_cards(&engine, &dir).await;
    assert_eq!(
        engine.hydrate_card_for_outcome(error_id).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, error_id),
        Some(SemanticState::VerifiedFixed {
            by: Some(by_item.id)
        }),
        "the obligation settles once the evidence is readable again"
    );
    assert!(
        state.pending_cold_semantic_intents.is_empty(),
        "the resolved obligation is consumed"
    );
}

/// B-1 反例 4：输入超过截断副本长度且否定/保留修订在尾部。记录点用完整
/// 输入判定一次：保留性措辞不是撤销——冷路径不记意图、不撤销；热路径
/// 同一结论。
#[tokio::test]
async fn a_tail_retention_revision_beyond_the_truncated_copy_keeps_the_cold_decision_live() {
    // ~6000 chars 的纯小写填充（无实体形状），保留修订落在 4000 字符副本之外。
    let padding = "filler word ".repeat(500);
    let message = format!("Remove AuthService.rs. {padding}keep the 5-second timeout in place");
    assert!(
        message.chars().count() > 4200,
        "setup: the tail is beyond the copy"
    );
    // 冷路径。
    {
        let dir = tempfile::tempdir().unwrap();
        let first = SimpleContextEngine::new(cold_config(&dir));
        let _task = open_focus(&first, "tail retention").await;
        first
            .ingest(ContextIngress::UserMessage {
                content: OLD.into(),
            })
            .await
            .unwrap();
        let old_id = externalize_heap_item(&first, &dir, |item| item.content == OLD).await;
        let engine = recapture_with_pending_cards(&first, &dir).await;
        engine
            .ingest(ContextIngress::UserMessage {
                content: message.clone(),
            })
            .await
            .unwrap();
        {
            let state = engine.state.lock().await;
            assert!(
                state.pending_cold_semantic_intents.is_empty(),
                "a retention-protected message is no withdrawal at all: nothing is recorded"
            );
        }
        assert_eq!(
            engine.hydrate_card_for_outcome(old_id).await.outcome,
            PendingIdOutcome::Installed
        );
        let state = engine.state.lock().await;
        assert_eq!(
            semantic_at(&state, old_id),
            Some(SemanticState::Live),
            "the cold path must reach the same conclusion as the loaded path: keep"
        );
    }
    // 热（已加载）路径：同一条输入对已加载决策保留——两条路径同源。
    {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        let _task = open_focus(&engine, "tail retention loaded").await;
        engine
            .ingest(ContextIngress::UserMessage {
                content: OLD.into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::UserMessage { content: message })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await
            .unwrap();
        assert!(
            report
                .transitions
                .iter()
                .all(|t| !t.reason.contains("superseded by decision")),
            "the loaded path keeps the decision: {:?}",
            report
                .transitions
                .iter()
                .map(|t| &t.reason)
                .collect::<Vec<_>>()
        );
        let state = engine.state.lock().await;
        let old = state
            .items
            .iter()
            .find(|item| item.content.contains("5-second timeout"))
            .expect("the retained decision stays addressable");
        assert_eq!(old.semantic, SemanticState::Live);
    }
}

/// B-1 反例 5：处理中途 checkpoint/restore。已结算目标保持终态（卡片带
/// 着终态重入，幂等），未结算义务随意图持久化并可继续。
#[tokio::test]
async fn a_midway_checkpoint_restore_keeps_settled_targets_and_open_obligations() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, [id_a, id_b], new_id) = engine_with_two_pending_old_decisions(&dir).await;
    assert_eq!(
        engine.hydrate_card_for_outcome(id_a).await.outcome,
        PendingIdOutcome::Installed
    );
    // 结算账本随 checkpoint 持久化：候选只剩 B，A 已进已结算集合。
    let value = engine.checkpoint().await.unwrap();
    let view = &intent_payload(&value, 0)["target_view"];
    assert_eq!(
        view["candidates"].as_array().map(Vec::len),
        Some(1),
        "the record-time candidate snapshot shrank to the unresolved target"
    );
    assert_eq!(
        view["settled"].as_array().map(Vec::len),
        Some(1),
        "the settled target is accounted"
    );
    let engine = SimpleContextEngine::new(cold_config(&dir));
    engine.restore(value).await.unwrap();
    {
        let state = engine.state.lock().await;
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "restore keeps the open obligation"
        );
        assert!(
            state.external.get(id_a).is_none(),
            "setup: the settled target's card is pending again (batch 0)"
        );
    }
    // 已结算目标重入：卡片带着终态装回，幂等——不重复结算、义务保留。
    assert_eq!(
        engine.hydrate_card_for_outcome(id_a).await.outcome,
        PendingIdOutcome::Installed
    );
    {
        let state = engine.state.lock().await;
        assert_eq!(
            semantic_at(&state, id_a),
            Some(SemanticState::Superseded { by: Some(new_id) }),
            "the settled target keeps its terminal state"
        );
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "idempotent re-entry does not finish the obligation: B is still open"
        );
    }
    // 未结算义务继续：B 安装即结算。
    assert_eq!(
        engine.hydrate_card_for_outcome(id_b).await.outcome,
        PendingIdOutcome::Installed
    );
    let state = engine.state.lock().await;
    assert_eq!(
        semantic_at(&state, id_b),
        Some(SemanticState::Superseded { by: Some(new_id) })
    );
    assert!(state.pending_cold_semantic_intents.is_empty());
}

/// B-1 反例 6：目标记账有界且诚实。记录时点候选快照超上限 → 截断标志＋
/// 计数，且被截掉的目标依然被结算（结算与完成记账分离）；已结算集合到
/// 上限 → 溢出计数，结算本身不受影响。
#[tokio::test]
async fn intent_target_accounting_stays_bounded_and_honest() {
    // 腿 A：记录时快照截断。
    {
        let dir = tempfile::tempdir().unwrap();
        let (engine, old_id, _task) =
            engine_with_old_decision_at(&dir, Position::PendingColdCard).await;
        {
            // 把真实目标挤出到 64 行快照窗口之外。
            let mut state = engine.state.lock().await;
            let fakes: Vec<(ContextItemId, String)> = (0..64)
                .map(|_| (ContextItemId::new(), "fake-card".into()))
                .collect();
            state.pending_external_cards.splice(..0, fakes);
        }
        engine
            .ingest(ContextIngress::UserMessage {
                content: REMOVE.into(),
            })
            .await
            .unwrap();
        let value = engine.checkpoint().await.unwrap();
        let view = &intent_payload(&value, 0)["target_view"];
        assert_eq!(
            view["candidates"].as_array().map(Vec::len),
            Some(64),
            "the candidate snapshot is bounded"
        );
        assert_eq!(
            view["candidates_truncated"].as_bool(),
            Some(true),
            "the truncation is an honest recorded fact"
        );
        assert_eq!(
            value["cold_semantic_intent_snapshots_truncated"].as_u64(),
            Some(1),
            "the overflow is counted, never hidden"
        );
        // 被截掉的目标依然被结算——结算谓词不依赖候选记账。
        assert_eq!(
            engine.hydrate_card_for_outcome(old_id).await.outcome,
            PendingIdOutcome::Installed
        );
        let state = engine.state.lock().await;
        let new_id = state
            .items
            .iter()
            .find(|item| item.content == REMOVE)
            .expect("the removing decision exists")
            .id;
        assert_eq!(
            semantic_at(&state, old_id),
            Some(SemanticState::Superseded { by: Some(new_id) }),
            "settlement does not depend on the accounting snapshot"
        );
        assert_eq!(
            state.pending_cold_semantic_intents.len(),
            1,
            "a truncated snapshot never claims completion (the ring governs retention)"
        );
    }
    // 腿 B：已结算集合有界，溢出诚实计数且不影响结算本身。
    {
        let dir = tempfile::tempdir().unwrap();
        let (engine, old_id, _task) =
            engine_with_old_decision_at(&dir, Position::PendingColdCard).await;
        engine
            .ingest(ContextIngress::UserMessage {
                content: REMOVE.into(),
            })
            .await
            .unwrap();
        let mut value = engine.checkpoint().await.unwrap();
        {
            // 直接把已结算集合填到上限（16 个互不相同的 id，避免命中幂等
            // 去重）：溢出必须被计数，且结算照常发生。
            let settled: Vec<serde_json::Value> = (0..16)
                .map(|_| serde_json::Value::String(ContextItemId::new().to_string()))
                .collect();
            intent_payload_mut(&mut value, 0)["target_view"]["settled"] =
                serde_json::Value::Array(settled);
        }
        let engine = SimpleContextEngine::new(cold_config(&dir));
        engine.restore(value).await.unwrap();
        assert_eq!(
            engine.hydrate_card_for_outcome(old_id).await.outcome,
            PendingIdOutcome::Installed
        );
        let state = engine.state.lock().await;
        let new_id = state
            .items
            .iter()
            .find(|item| item.content == REMOVE)
            .expect("the removing decision exists")
            .id;
        assert_eq!(
            semantic_at(&state, old_id),
            Some(SemanticState::Superseded { by: Some(new_id) }),
            "overflow of the bounded accounting set never blocks the real settlement"
        );
        assert!(
            state.pending_cold_semantic_intents.is_empty(),
            "every record-time candidate was observed: the obligation completes"
        );
        drop(state);
        // State 级诚实计数随 checkpoint 序列化，从真实引擎导出审计。
        let audit = engine.checkpoint().await.unwrap();
        assert_eq!(
            audit["cold_semantic_intent_settlement_overflows"].as_u64(),
            Some(1),
            "the settled-set overflow is counted, never hidden"
        );
    }
}

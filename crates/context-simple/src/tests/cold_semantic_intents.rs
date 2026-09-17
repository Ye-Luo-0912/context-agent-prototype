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

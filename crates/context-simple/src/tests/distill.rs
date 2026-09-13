use std::sync::Arc;

use agent_contracts::{
    AgentResult, ContextEngine, ContextHints, ContextIngress, ContextKind,
    ContextMaintenanceTrigger, ContextQuery,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

/// Long-task acceptance (`long_task_10k_turns`): over 10,000 task turns the
/// resident working set is bounded by the current episode plus unresolved
/// semantic state, not by turn count. Required decisions stay recallable;
/// stale ordinary dialogue leaves Resident.
#[tokio::test]
async fn long_task_10k_turns_keeps_the_working_set_episode_bounded() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Test-only semantic boundary: consecutive per-turn messages from
        // different workstreams share almost no tokens, so the episode
        // rotates on the semantic signal (the default threshold is
        // deliberately more conservative).
        episode_rotate_threshold: 0.35,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "maintain the auth service").await;

    // Turn 0: a durable decision the task must keep recalling.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "0", "reviewed AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    // Most turns come from different workstreams (semantic rotation); the
    // final burst is a stream of related messages (turn-budget rotation).
    const WORKSTREAMS: &[&str] = &[
        "fix the auth cache invalidation",
        "refactor retry backoff for shards",
        "add request tracing to the gateway",
        "tune connection pool sizing",
        "investigate the token bucket throttle",
        "rework circuit breaker thresholds",
        "profile event bus dispatch latency",
        "harden the input validation path",
        "reduce index rebuild cost",
        "document the deployment runbook",
    ];
    let mut max_resident = 0usize;
    let mut max_resident_bytes = 0usize;
    let mut early_ordinary_id = None;
    let mut resident_at_2000 = 0usize;
    let mut resident_bytes_at_2000 = 0usize;
    for turn in 1..=10_000u64 {
        let content = if turn <= 9_000 {
            format!(
                "{} in round {}",
                WORKSTREAMS[turn as usize % WORKSTREAMS.len()],
                turn
            )
        } else {
            // Related messages: the semantic signal never fires, so the
            // episode must rotate on the 500-turn budget instead.
            format!("keep working on the auth cache and the retry backoff in round {turn}")
        };
        engine
            .ingest(ContextIngress::UserMessage { content })
            .await
            .unwrap();
        tool_observation(
            &engine,
            &turn.to_string(),
            &format!("patched Item{}", turn % 13),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if turn % 50 == 0 {
            engine.gc().await.unwrap();
            let state = engine.state.lock().await;
            let resident = state.items.len();
            let resident_bytes: usize = state.items.iter().map(|item| item.content.len()).sum();
            drop(state);
            max_resident = max_resident.max(resident);
            max_resident_bytes = max_resident_bytes.max(resident_bytes);
            if turn == 2_000 {
                resident_at_2000 = resident;
                resident_bytes_at_2000 = resident_bytes;
            }
        }
        // Record the turn-100 ordinary message while it is still resident
        // (it is evicted by the very next episode rotation).
        if turn == 100 {
            early_ordinary_id = engine
                .state
                .lock()
                .await
                .items
                .iter()
                .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 100)
                .map(|item| item.id);
        }
    }

    // 1. Bounded working set: without episode rotation 10,000 turns would
    // leave ~20,000 resident items; rotation keeps the peak to the current
    // episode plus hot recalls (the 500-turn budget burst is dominated by
    // GC and never accumulates).
    assert!(
        max_resident < 200,
        "resident working set must stay bounded, peak was {max_resident}"
    );
    // 2. Bounded *over time*: the working set must not grow with turn
    // count. A linear-growth engine would show a large delta between turn
    // 2,000 and turn 10,000.
    let resident_at_10000 = engine.state.lock().await.items.len();
    assert!(
        resident_at_10000 <= resident_at_2000.saturating_add(20),
        "the working set must not grow with turn count: {resident_at_2000} -> {resident_at_10000}"
    );
    // 3. Resident *bytes* flatten too: a smaller item count must not hide
    // a growing heap. Same 20% growth allowance as the count check, plus
    // a small absolute slack for variable message length.
    let resident_bytes_at_10000: usize = engine
        .state
        .lock()
        .await
        .items
        .iter()
        .map(|item| item.content.len())
        .sum();
    assert!(
        max_resident_bytes < 80_000,
        "resident heap bytes must stay bounded, peak was {max_resident_bytes}"
    );
    let byte_slack = resident_bytes_at_2000 / 5 + 4_096;
    assert!(
        resident_bytes_at_10000 <= resident_bytes_at_2000.saturating_add(byte_slack),
        "resident bytes must not grow with turn count: {resident_bytes_at_2000} -> {resident_bytes_at_10000}"
    );

    // 2. Stale ordinary dialogue leaves Resident.
    let early = early_ordinary_id.expect("an early ordinary message id");
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|item| item.id == early),
            "stale ordinary dialogue must leave the resident heap"
        );
    }

    // 3. The required decision stays recallable: touch its entity, then
    // materialize and expect it back in the working set.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we decide about AuthService.rs?".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "final", "touched AuthService.rs again").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();

    let materialized = engine
        .materialize(ContextQuery {
            current_input: "what did we decide about AuthService.rs?".into(),
            budget_tokens: 8192,
            hints: ContextHints::default(),
        })
        .await
        .unwrap();
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.kind == ContextKind::UserMessage
                && item.content.contains("AuthService.rs")),
        "the required decision must stay recallable, selected: {:?}",
        materialized
            .items
            .iter()
            .map(|item| &item.content)
            .collect::<Vec<_>>()
    );
}

/// Cadence regression for the episode-local turn budget: one overlong
/// episode that rotates on the `episode_max_user_turns` guard must not
/// permanently exhaust every later episode's budget. The rotation resets
/// the counter, so the next episode's related messages do not rotate until
/// their own turn budget is exhausted — without the reset the guard fires
/// on the very next user message and rotates a fresh single-turn episode.
#[tokio::test]
async fn one_overlong_episode_does_not_exhaust_later_episode_budgets() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        // Related messages never fire the semantic signal (threshold 0
        // means overlap can never fall below it), so only the turn budget
        // can rotate the episode.
        episode_rotate_threshold: 0.0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "keep working on the auth service").await;

    // Drive the episode past its turn budget. `FocusState.generation` is
    // bumped once by `FocusChanged`, so the guard fires at turn `max_turns`
    // (the budget itself): the counter reaches the cap on the last
    // in-budget message and the next message observes it.
    let max_turns = SimpleContextConfig::default().episode_max_user_turns as u64;
    let mut rotated_at: Option<u64> = None;
    for turn in 1..=max_turns + 1 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if report
            .transitions
            .iter()
            .any(|t| t.reason.contains("episode rotated"))
            && rotated_at.is_none()
        {
            rotated_at = Some(turn);
        }
    }
    // The overlong episode survives its full budget: the guard fires at
    // the budget boundary, not immediately.
    let rotated_at = rotated_at.expect("the turn-budget guard must rotate the overlong episode");
    assert!(
        rotated_at >= max_turns.saturating_sub(1),
        "the episode must survive its full turn budget before rotating, rotated at turn {rotated_at}"
    );

    // A fresh episode starts with a reset budget: five related messages
    // must not rotate again (a rotation would evict this episode's
    // ordinary dialogue), and the dialogue stays resident.
    let mut resident_turns = Vec::new();
    for turn in 1..=5u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert!(
            !report
                .transitions
                .iter()
                .any(|t| t.reason.contains("episode rotated")),
            "a fresh episode must not rotate on the exhausted-budget guard (round {turn})"
        );
        resident_turns = engine
            .state
            .lock()
            .await
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::UserMessage)
            .map(|item| item.created_turn)
            .collect();
    }
    assert!(
        resident_turns.len() >= 5,
        "the fresh episode's ordinary dialogue must stay resident, got {resident_turns:?}"
    );
}

// ---------------------------------------------------------------------------
// CTX-3/E04: provenance honesty for episode cards.
// ---------------------------------------------------------------------------

use agent_contracts::{BoundedCompactor, CompactionOutput, CompactionRequest};

struct FixedCard;
#[async_trait::async_trait]
impl BoundedCompactor for FixedCard {
    async fn compact(&self, _request: CompactionRequest) -> AgentResult<CompactionOutput> {
        Ok(CompactionOutput {
            text: "[episode card] scripted summary".into(),
            input_tokens: 0,
            output_tokens: 0,
            usage_identity: agent_contracts::UsageIdentity::Observed,
            cached_input_tokens: None,
            cache_write_input_tokens: None,
            cache_miss_input_tokens: None,
            attempts: 0,
            retries: 0,
        })
    }
}

struct FailingCompactor;
#[async_trait::async_trait]
impl BoundedCompactor for FailingCompactor {
    async fn compact(
        &self,
        _request: CompactionRequest,
    ) -> agent_contracts::AgentResult<CompactionOutput> {
        Err(agent_contracts::AgentError::Model("compactor down".into()))
    }
}

/// COST-7 (R2-11): an empty summary is a REFUSED fold but a BILLED call —
/// the typed error carries the provider's report so the account can keep
/// the real counters under their honest identity.
struct EmptySummaryWithUsage;
#[async_trait::async_trait]
impl BoundedCompactor for EmptySummaryWithUsage {
    async fn compact(
        &self,
        _request: CompactionRequest,
    ) -> agent_contracts::AgentResult<CompactionOutput> {
        Err(agent_contracts::AgentError::EmptyCompactionSummary {
            usage: agent_contracts::ModelUsage {
                input_tokens: Some(140),
                output_tokens: Some(9),
                cached_input_tokens: Some(90),
                attempts: 1,
                retries: 0,
                ..Default::default()
            },
        })
    }
}

fn distill_engine(
    compactor: impl BoundedCompactor + 'static,
    max_turns: usize,
) -> SimpleContextEngine {
    SimpleContextEngine::new(SimpleContextConfig {
        episode_rotate_threshold: 0.0,
        episode_max_user_turns: max_turns,
        force_episode_llm_distill: true,
        ..SimpleContextConfig::default()
    })
    .with_compactor(Arc::new(compactor))
}

fn episode_card(state: &crate::engine::State) -> &agent_contracts::ContextItem {
    state
        .items
        .iter()
        .rev()
        .find(|item| item.source.as_deref() == Some("episode-derived"))
        .expect("an episode card must exist")
}

fn long_message(tag: &str, chars: usize) -> String {
    let mut body = format!("{tag} ");
    while body.chars().count() < chars {
        body.push('x');
    }
    body
}

/// E04 反例：第一条来源超预算（截断进输入）时，后续来源不得进入
/// DerivedFrom——来源关系不得指向压缩器没读过的条目；排除事实在卡片
/// 上可见，被排除条目保持 live 可检索。
#[tokio::test]
async fn provenance_never_names_sources_the_compactor_did_not_read() {
    let engine = distill_engine(FixedCard, 3);
    open_focus(&engine, "rotate the auth episode").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: long_message("OVERSIZED first source", 2_100),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "calibrate the frobnicator after the swap".into(),
        })
        .await
        .unwrap();

    // A third message triggers the rotation (the budget guard observes the
    // cap on the NEXT message): members are the first two sources, packed
    // newest-first — the short second source fits, the oversized first one
    // does not and is excluded.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger the rotation".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let card = episode_card(&state);
    let oversized_id = state
        .items
        .iter()
        .find(|item| item.content.contains("OVERSIZED"))
        .expect("the oversized source exists")
        .id;
    let frobnicator_id = state
        .items
        .iter()
        .find(|item| item.content.contains("frobnicator"))
        .expect("the second source exists")
        .id;
    assert_eq!(
        card.dependencies.len(),
        1,
        "only the source that entered the input carries DerivedFrom: {:?}",
        card.dependencies
    );
    assert!(
        card.dependencies
            .iter()
            .any(|edge| edge.target == frobnicator_id),
        "the source that entered the input is the provenance"
    );
    assert!(
        !card
            .dependencies
            .iter()
            .any(|edge| edge.target == oversized_id),
        "the source the compactor never read carries no DerivedFrom edge"
    );
    assert!(
        card.content.contains("covers 1 of 2 sources"),
        "the coverage omission must be visible on the card: {}",
        card.content
    );
    let oversized = state
        .items
        .iter()
        .find(|item| item.id == oversized_id)
        .unwrap();
    assert!(
        oversized.semantic.is_live(),
        "the excluded source stays live"
    );
}

/// 9 条来源、预算与来源上限内最多带 8 条：第 9 条（最旧）排除且计数可见。
#[tokio::test]
async fn nine_sources_take_the_newest_eight_within_budget() {
    let engine = distill_engine(FixedCard, 9);
    open_focus(&engine, "rotate a wide episode").await;
    let mut first_id = None;
    for turn in 1..=10u64 {
        let content = long_message(&format!("source-{turn}"), 250);
        engine
            .ingest(ContextIngress::UserMessage { content })
            .await
            .unwrap();
        if turn == 1 {
            let state = engine.state.lock().await;
            first_id = Some(
                state
                    .items
                    .iter()
                    .find(|item| item.content.contains("source-1 "))
                    .expect("first source")
                    .id,
            );
        }
    }
    let state = engine.state.lock().await;
    let card = episode_card(&state);
    assert_eq!(
        card.dependencies.len(),
        7,
        "the 2000-char budget packs seven 250-char sources: {:?}",
        card.dependencies.len()
    );
    assert!(
        !card
            .dependencies
            .iter()
            .any(|edge| edge.target == first_id.unwrap()),
        "the oldest source beyond the budget is excluded"
    );
    assert!(
        card.content.contains("covers 7 of 8 sources"),
        "{}",
        card.content
    );
}

/// 三次旋转：每张新卡的输入实际包含前卡（累计笔记），替代因此有覆盖
/// 依据——前卡被 superseded 且出现在新卡 DerivedFrom 里。
#[tokio::test]
async fn three_rotations_chain_cards_with_coverage() {
    let engine = distill_engine(FixedCard, 2);
    open_focus(&engine, "chain episodes").await;
    for turn in 1..=7u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("chain step {turn} for the auth rotation"),
            })
            .await
            .unwrap();
        // The queued supersession (prior card covered by the new card) is
        // applied by the next maintain pass.
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }
    let state = engine.state.lock().await;
    let cards: Vec<&agent_contracts::ContextItem> = state
        .items
        .iter()
        .filter(|item| item.source.as_deref() == Some("episode-derived"))
        .collect();
    assert!(
        cards.len() >= 3,
        "three rotations must produce three cards: {}",
        cards.len()
    );
    for pair in cards.windows(2) {
        let (older, newer) = (pair[0], pair[1]);
        assert!(
            newer.dependencies.iter().any(|edge| {
                edge.target == older.id && edge.kind == agent_contracts::DependencyKind::DerivedFrom
            }),
            "the newer card must chain to the prior card with coverage"
        );
        assert!(
            !older.semantic.is_live(),
            "a covered prior card is superseded: {:?}",
            older.semantic
        );
    }
}

/// 前卡装不进预算时不被替代：保持 live、不在新卡来源里——旧卡退役
/// 必须有覆盖依据。
#[tokio::test]
async fn a_prior_card_that_does_not_fit_stays_live_and_unsuperseded() {
    let engine = distill_engine(FixedCard, 2);
    open_focus(&engine, "chain episodes with a big card").await;
    for turn in 1..=2u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("seed step {turn}"),
            })
            .await
            .unwrap();
    }
    let first_card_id = {
        let state = engine.state.lock().await;
        episode_card(&state).id
    };
    engine
        .ingest(ContextIngress::UserMessage {
            content: long_message("OVERSIZED replacement episode", 2_100),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger the second rotation".into(),
        })
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let old_card = state
        .items
        .iter()
        .find(|item| item.id == first_card_id)
        .expect("the prior card stays addressable");
    assert!(
        old_card.semantic.is_live(),
        "a prior card that never entered the input must not be superseded: {:?}",
        old_card.semantic
    );
    let card = episode_card(&state);
    assert!(
        !card
            .dependencies
            .iter()
            .any(|edge| edge.target == first_card_id),
        "the unread prior card carries no DerivedFrom edge"
    );
}

/// 压缩失败：fallback 卡明示「未完成压缩」，原文随卡可检索；来源关系
/// 仍只指向实际进入输入的来源。
#[tokio::test]
async fn failed_compaction_card_says_it_is_incomplete() {
    let engine = distill_engine(FailingCompactor, 1);
    open_focus(&engine, "rotate with a down compactor").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "the one oversized source for the failing compactor".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger the rotation anyway".into(),
        })
        .await
        .unwrap();
    let state = engine.state.lock().await;
    let card = episode_card(&state);
    assert!(
        card.content.starts_with("[episode distill incomplete"),
        "the fallback card must admit the compaction did not happen: {}",
        card.content
    );
    assert!(
        card.content.contains("the one oversized source"),
        "the raw source stays attached and retrievable: {}",
        card.content
    );
}

/// COST-7 (R2-11)：空摘要的压缩调用——折叠被拒绝，但已收到的 usage
/// 按 Observed 身份进入账目，不再随 Err 消失成一条不存在的记录。
#[tokio::test]
async fn an_empty_summary_distill_keeps_the_reported_usage_in_the_ledger() {
    let engine = distill_engine(EmptySummaryWithUsage, 1);
    open_focus(&engine, "rotate with an empty-summary compactor").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: long_message("oversized empty-summary source", 2_100),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger the rotation".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let row = report
        .compactions
        .iter()
        .find(|row| row.reason == agent_contracts::CompactionReason::EpisodeRotation)
        .expect("the billed-but-refused distill must reach the ledger");
    assert_eq!(row.input_tokens, 140);
    assert_eq!(row.output_tokens, 9);
    assert_eq!(row.cached_input_tokens, Some(90));
    assert_eq!(
        row.usage_identity,
        agent_contracts::UsageIdentity::Observed,
        "a full provider report stays observed, not unknown"
    );
}

/// COST-7 (R2-11)：无证据的压缩失败带一条显式 Unknown 行进账本——旧
/// 「非零才入账」门槛让 Unknown 0/0 行整条消失，失败成本不可见。
#[tokio::test]
async fn a_failed_distill_still_lands_an_unknown_row_in_the_ledger() {
    let engine = distill_engine(FailingCompactor, 1);
    open_focus(&engine, "rotate with a down compactor for the ledger").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: long_message("oversized failing-compactor source", 2_100),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger the rotation".into(),
        })
        .await
        .unwrap();
    let report = engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let row = report
        .compactions
        .iter()
        .find(|row| row.reason == agent_contracts::CompactionReason::EpisodeRotation)
        .expect("a failed distill must not vanish from the ledger");
    assert_eq!(
        row.usage_identity,
        agent_contracts::UsageIdentity::Unknown,
        "no typed evidence means unknown, never an observed zero"
    );
}

/// 恢复后 provenance 不膨胀：checkpoint/restore 后卡的 DerivedFrom 数与
/// 目标集合不变。
#[tokio::test]
async fn restore_keeps_provenance_stable() {
    let engine = distill_engine(FixedCard, 1);
    open_focus(&engine, "rotate and restore").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: long_message("oversized restore source", 2_100),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "trigger before restore".into(),
        })
        .await
        .unwrap();
    let (before_deps, before_targets) = {
        let state = engine.state.lock().await;
        let card = episode_card(&state);
        (
            card.dependencies.len(),
            card.dependencies
                .iter()
                .map(|edge| edge.target)
                .collect::<Vec<_>>(),
        )
    };
    let snapshot = engine.checkpoint().await.unwrap();
    engine.restore(snapshot).await.unwrap();
    let state = engine.state.lock().await;
    let card = episode_card(&state);
    assert_eq!(card.dependencies.len(), before_deps);
    assert_eq!(
        card.dependencies
            .iter()
            .map(|edge| edge.target)
            .collect::<Vec<_>>(),
        before_targets
    );
}

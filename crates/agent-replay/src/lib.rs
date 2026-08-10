//! Deterministic replay of a context lifecycle from a JSONL event journal.
//!
//! The runtime records every context-relevant action as a `RuntimeEvent`
//! (`UserMessageAccepted`, `Pinned`, `ToolFinished`, `ContextMaintained`,
//! `ContextPrepared`, ...). Replay walks those events in order and drives a
//! fresh context engine with the exact same ingest / maintain /
//! materialize calls, then reports, per item:
//!
//! - what entered and why (`source`, `kind`, entry turn);
//! - which model turns consumed it (from `ContextConsumed`; legacy traces
//!   without that event retain the old `ContextPrepared` behavior);
//! - every state transition, with the maintenance turn and reason;
//! - the final state.
//!
//! The same machinery powers the A/B/C comparison: `scenarios` synthesizes
//! deterministic task traces and `compare_scenario` runs each trace through
//! the append-only, rolling-summary and dynamic-working-set engines to
//! measure input-token cost, over-budget turns and context churn.

mod facts;
mod scenarios;

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use agent_contracts::{
    AttentionState, ContextConsumptionAck, ContextDiagnostics, ContextEngine, ContextHints,
    ContextIngress, ContextItemId, ContextItemSummary, ContextKind, ContextQuery, ContextSelection,
    FocusState, MaterializedContext, MaterializedItem, OperationId, RuntimeEvent,
    RuntimeEventEnvelope, TurnId, tokens,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};

pub use facts::{
    FactCoverage, FactOutcome, KeyFact, compare_facts, render_fact_comparison, scenario_key_facts,
};
pub use scenarios::{
    Scenario, all_scenarios, compare_config, compare_scenario, engine_variants, render_comparison,
};

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub system_prompt: String,
    pub budget_tokens: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            system_prompt: concat!(
                "You are a focused coding agent. Work on the current task only. ",
                "Treat SELECTED WORKING CONTEXT as a bounded cache, not a complete transcript. ",
                "Use tools when needed. Do not assume omitted history is relevant."
            )
            .to_string(),
            budget_tokens: 24_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplayedTransition {
    pub turn: u64,
    pub from: AttentionState,
    pub to: AttentionState,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ReplayedItem {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: agent_contracts::ContextScope,
    pub attention: AttentionState,
    pub source: Option<String>,
    pub created_turn: u64,
    pub access_count: u32,
    /// Ids of prior items this item explicitly depends on (shared entities).
    pub dependencies: Vec<ContextItemId>,
    pub consumed_turns: Vec<u64>,
    pub transitions: Vec<ReplayedTransition>,
}

#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub events_consumed: usize,
    pub items: Vec<ReplayedItem>,
    pub turns: u64,
    pub tool_rounds: u64,
    pub snapshot_builds: usize,
    pub final_diagnostics: ContextDiagnostics,
    /// Measurement: total input tokens across all snapshot builds.
    pub input_tokens_total: usize,
    /// Measurement: largest single snapshot (worst single model request).
    pub input_tokens_max: usize,
    /// Measurement: snapshots that exceeded the configured budget.
    pub over_budget_snapshots: usize,
    /// Measurement: total lifecycle transitions emitted by maintenance.
    pub transitions_total: usize,
    /// Measurement: total GC evictions / reactivations (reversible, so they
    /// are part of the lifecycle story, not an error).
    pub gc_evictions: usize,
    pub gc_reactivations: usize,
}

/// Replay a slice of envelopes through the dynamic working-set engine.
/// Deterministic: same events, same engine version, same story.
pub async fn replay_events(
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<ReplayOutcome> {
    let engine = Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    run_engine(engine, events, config).await
}

/// Replay a slice of envelopes through any `ContextEngine` implementation and
/// collect lifecycle + token-cost measurements. Used by both single-engine
/// replay and the A/B/C comparison.
pub async fn run_engine(
    engine: Arc<dyn ContextEngine>,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<ReplayOutcome> {
    run_engine_observing(engine, events, config, |_, _| {}).await
}

/// Like [`run_engine`], but hands every materialized snapshot to an
/// observer alongside the turn that produced it. The observer sees exactly
/// the working-set items the model would have received (`MaterializedItem`
/// carries content), which is what the fact-coverage evaluation keys on.
pub(crate) async fn run_engine_observing(
    engine: Arc<dyn ContextEngine>,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    mut observe: impl FnMut(u64, &[MaterializedItem]),
) -> anyhow::Result<ReplayOutcome> {
    struct PendingReplayContext {
        materialized: MaterializedContext,
        selected_item_ids: Vec<ContextItemId>,
    }

    let mut current_input = String::new();
    let mut current_turn = 0u64;
    let mut total_tool_rounds = 0u64;
    let mut consumed: HashMap<ContextItemId, Vec<u64>> = HashMap::new();
    let mut transitions: HashMap<ContextItemId, Vec<ReplayedTransition>> = HashMap::new();
    let mut snapshot_builds = 0usize;
    let mut events_consumed = 0usize;
    let mut input_tokens_total = 0usize;
    let mut input_tokens_max = 0usize;
    let mut over_budget_snapshots = 0usize;
    let mut transitions_total = 0usize;
    let mut gc_evictions = 0usize;
    let mut gc_reactivations = 0usize;
    // New traces distinguish a preview from a successful provider
    // consumption. A whole-trace feature check keeps old JSONL journals and
    // synthetic benchmark scenarios replayable without inventing ack events.
    let has_explicit_consumption = events
        .iter()
        .any(|envelope| matches!(envelope.event, RuntimeEvent::ContextConsumed { .. }));
    let mut pending_context: Option<PendingReplayContext> = None;

    for envelope in events {
        events_consumed += 1;
        match &envelope.event {
            RuntimeEvent::UserMessageAccepted { content } => {
                current_turn += 1;
                current_input = content.clone();
                engine
                    .ingest(ContextIngress::UserMessage {
                        content: content.clone(),
                    })
                    .await?;
            }
            RuntimeEvent::FocusChanged { task_id, goal } => {
                // Replay preserves the task identity: re-focusing a task
                // with the same id resumes its scopes exactly like the live
                // runtime, instead of minting a fresh task per event.
                let focus = FocusState::for_task(*task_id, goal.clone());
                engine
                    .ingest(ContextIngress::FocusChanged { focus })
                    .await?;
            }
            RuntimeEvent::FocusCleared => {
                engine.ingest(ContextIngress::FocusCleared).await?;
            }
            RuntimeEvent::Pinned { content } => {
                engine
                    .ingest(ContextIngress::Pin {
                        content: content.clone(),
                        kind: ContextKind::Constraint,
                    })
                    .await?;
            }
            RuntimeEvent::TaskCompleted { summary, .. } => {
                engine
                    .ingest(ContextIngress::TaskCompleted {
                        task_id: None,
                        summary: summary.clone(),
                    })
                    .await?;
            }
            RuntimeEvent::AssistantMessage { content } => {
                engine
                    .ingest(ContextIngress::AssistantMessage {
                        content: content.clone(),
                    })
                    .await?;
            }
            RuntimeEvent::ToolFinished { output } => {
                total_tool_rounds += 1;
                engine
                    .ingest(ContextIngress::ToolObservation {
                        output: output.clone(),
                        scope_id: None,
                    })
                    .await?;
            }
            RuntimeEvent::ContextMaintained { trigger, .. } => {
                let report = engine.maintain(*trigger).await?;
                transitions_total += report.transitions.len();
                for transition in report.transitions {
                    transitions
                        .entry(transition.item_id)
                        .or_default()
                        .push(ReplayedTransition {
                            turn: transition.turn,
                            from: transition.from,
                            to: transition.to,
                            reason: transition.reason,
                        });
                }
            }
            RuntimeEvent::ContextPrepared { selected, .. } => {
                let materialized = engine
                    .materialize(ContextQuery {
                        current_input: current_input.clone(),
                        budget_tokens: config.budget_tokens,
                        hints: ContextHints::default(),
                    })
                    .await?;
                snapshot_builds += 1;
                // The materialized share plus the runtime-owned system prompt
                // is what the model request actually pays for.
                let input_tokens =
                    tokens::approx_tokens(&config.system_prompt) + materialized.approx_tokens;
                input_tokens_total += input_tokens;
                input_tokens_max = input_tokens_max.max(input_tokens);
                if input_tokens > config.budget_tokens {
                    over_budget_snapshots += 1;
                }
                if has_explicit_consumption {
                    // Runtime ids are random and cannot be replayed directly.
                    // ContextPrepared carries the final selection metadata in
                    // engine order, so map that subsequence onto the fresh
                    // engine's preview and wait for ContextConsumed to commit.
                    let selected_item_ids = map_recorded_selection(&materialized, selected)?;
                    pending_context = Some(PendingReplayContext {
                        materialized,
                        selected_item_ids,
                    });
                } else {
                    // Compatibility for journals written before consumption
                    // acknowledgements existed: materialize used to reinforce
                    // its whole preview immediately.
                    let ack = ContextConsumptionAck {
                        turn_id: TurnId::new(),
                        operation_id: OperationId::new(),
                        model_round: snapshot_builds,
                        materialization_id: materialized.materialization_id,
                        item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
                        external_item_ids: materialized
                            .external
                            .iter()
                            .map(|entry| entry.item_id)
                            .collect(),
                    };
                    engine.acknowledge_consumption(ack).await?;
                    observe(current_turn, &materialized.items);
                    for item in materialized.items {
                        consumed.entry(item.item_id).or_default().push(current_turn);
                    }
                }
            }
            RuntimeEvent::ContextConsumed { ack } => {
                let pending = pending_context.take().ok_or_else(|| {
                    anyhow::anyhow!(
                        "ContextConsumed for operation {} has no pending ContextPrepared preview",
                        ack.operation_id
                    )
                })?;
                if ack.item_ids.len() != pending.selected_item_ids.len() {
                    anyhow::bail!(
                        "ContextConsumed item count {} differs from the replayed final selection count {}",
                        ack.item_ids.len(),
                        pending.selected_item_ids.len()
                    );
                }
                if ack.external_item_ids.len() != pending.materialized.external.len() {
                    anyhow::bail!(
                        "ContextConsumed external-ref count {} differs from the replayed preview count {}",
                        ack.external_item_ids.len(),
                        pending.materialized.external.len()
                    );
                }
                let local_ack = ContextConsumptionAck {
                    turn_id: ack.turn_id,
                    operation_id: ack.operation_id,
                    model_round: ack.model_round,
                    materialization_id: pending.materialized.materialization_id,
                    item_ids: pending.selected_item_ids.clone(),
                    external_item_ids: pending
                        .materialized
                        .external
                        .iter()
                        .map(|entry| entry.item_id)
                        .collect(),
                };
                engine.acknowledge_consumption(local_ack).await?;

                let selected_ids: HashSet<_> = pending.selected_item_ids.iter().copied().collect();
                let visible: Vec<_> = pending
                    .materialized
                    .items
                    .into_iter()
                    .filter(|item| selected_ids.contains(&item.item_id))
                    .collect();
                observe(current_turn, &visible);
                for item_id in pending.selected_item_ids {
                    consumed.entry(item_id).or_default().push(current_turn);
                }
            }
            RuntimeEvent::ContextGc { .. } => {
                // Replay the GC pass exactly like the runtime does at turn
                // boundaries, and count what it evicted/reactivated so the
                // report can explain the residency story.
                let gc_report = engine.gc().await?;
                gc_evictions += gc_report.evictions.len();
                gc_reactivations += gc_report.reactivations.len();
            }
            _ => {}
        }
    }

    let final_diagnostics = engine.diagnostics().await?;
    let summaries = engine.inspect(usize::MAX).await?;
    let items = build_replayed_items(summaries, consumed, transitions);

    Ok(ReplayOutcome {
        events_consumed,
        items,
        turns: current_turn,
        tool_rounds: total_tool_rounds,
        snapshot_builds,
        final_diagnostics,
        input_tokens_total,
        input_tokens_max,
        over_budget_snapshots,
        transitions_total,
        gc_evictions,
        gc_reactivations,
    })
}

/// Map the final selection recorded by a live run onto a fresh replay
/// engine. Runtime budget packing only removes entries from the engine's
/// ordered preview, so the event must be an exact metadata subsequence.
fn map_recorded_selection(
    materialized: &MaterializedContext,
    recorded: &[ContextSelection],
) -> anyhow::Result<Vec<ContextItemId>> {
    let mut cursor = 0usize;
    let mut mapped = Vec::with_capacity(recorded.len());
    for expected in recorded {
        let Some((offset, local)) = materialized.selected[cursor..]
            .iter()
            .enumerate()
            .find(|(_, local)| selection_metadata_matches(local, expected))
        else {
            anyhow::bail!(
                "recorded ContextPrepared selection cannot be mapped to the replay preview (tokens={}, reason={:?})",
                expected.approx_tokens,
                expected.reason
            );
        };
        cursor += offset + 1;
        mapped.push(local.item_id);
    }
    Ok(mapped)
}

fn selection_metadata_matches(left: &ContextSelection, right: &ContextSelection) -> bool {
    left.approx_tokens == right.approx_tokens
        && left.reason == right.reason
        && (left.score - right.score).abs() <= f32::EPSILON
}

/// Read a JSONL trace file (one `RuntimeEventEnvelope` per line) and replay it.
/// If the file contains multiple runs, only the first run's events are used.
pub async fn replay_file(path: &Path, config: &ReplayConfig) -> anyhow::Result<ReplayOutcome> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| anyhow::anyhow!("read trace {}: {e}", path.display()))?;

    let mut envelopes: Vec<RuntimeEventEnvelope> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let envelope: RuntimeEventEnvelope = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("parse trace line {}: {e}", index + 1))?;
        envelopes.push(envelope);
    }

    let Some(first) = envelopes.first() else {
        anyhow::bail!("trace {} contains no events", path.display());
    };
    let run_id = first.run_id;
    envelopes.retain(|envelope| envelope.run_id == run_id);

    replay_events(&envelopes, config).await
}

fn build_replayed_items(
    summaries: Vec<ContextItemSummary>,
    mut consumed: HashMap<ContextItemId, Vec<u64>>,
    mut transitions: HashMap<ContextItemId, Vec<ReplayedTransition>>,
) -> Vec<ReplayedItem> {
    summaries
        .into_iter()
        .map(|summary| ReplayedItem {
            id: summary.id,
            kind: summary.kind,
            scope: summary.scope,
            attention: summary.attention,
            source: summary.source,
            created_turn: summary.created_turn,
            access_count: summary.access_count,
            dependencies: summary.dependencies,
            consumed_turns: consumed.remove(&summary.id).unwrap_or_default(),
            transitions: transitions.remove(&summary.id).unwrap_or_default(),
        })
        .collect()
}

/// Human-readable lifecycle report answering the acceptance questions.
pub fn render_report(outcome: &ReplayOutcome) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "replay: consumed {} events | {} turns | {} tool rounds | {} snapshot builds\n",
        outcome.events_consumed, outcome.turns, outcome.tool_rounds, outcome.snapshot_builds
    ));
    let diagnostics = &outcome.final_diagnostics;
    out.push_str(&format!(
        "final context: total={} active={} cooling={} archived={} dropped={} active~{} tok | resident={} evicted={} gc(evict={} react={})\n\n",
        diagnostics.total_items,
        diagnostics.active_items,
        diagnostics.cooling_items,
        diagnostics.archived_items,
        diagnostics.tombstoned_items,
        diagnostics.approx_active_tokens,
        diagnostics.resident_items,
        diagnostics.warm_items,
        outcome.gc_evictions,
        outcome.gc_reactivations,
    ));

    for item in &outcome.items {
        out.push_str(&format!(
            "item {} [{} | {:?} | {:?}] entered turn {} (source: {})\n",
            short_id(&item.id),
            debug_kind(item.kind),
            item.scope,
            item.attention,
            item.created_turn,
            item.source.as_deref().unwrap_or("-"),
        ));
        if !item.dependencies.is_empty() {
            out.push_str(&format!(
                "  depends on: {}\n",
                item.dependencies
                    .iter()
                    .map(short_id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !item.consumed_turns.is_empty() {
            out.push_str(&format!(
                "  consumed by turns: {}\n",
                join_turns(&item.consumed_turns)
            ));
        }
        for transition in &item.transitions {
            out.push_str(&format!(
                "  turn {}: {:?} -> {:?}: {}\n",
                transition.turn, transition.from, transition.to, transition.reason
            ));
        }
        out.push_str(&format!("  final state: {:?}\n", item.attention));
    }
    out
}

fn short_id(id: &ContextItemId) -> String {
    id.to_string().chars().take(8).collect()
}

fn debug_kind(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::Goal => "Goal",
        ContextKind::Constraint => "Constraint",
        ContextKind::Decision => "Decision",
        ContextKind::UserMessage => "UserMessage",
        ContextKind::AssistantMessage => "AssistantMessage",
        ContextKind::ToolObservation => "ToolObservation",
        ContextKind::FileObservation => "FileObservation",
        ContextKind::Error => "Error",
        ContextKind::Summary => "Summary",
        ContextKind::Note => "Note",
    }
}

fn join_turns(turns: &[u64]) -> String {
    turns
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ContextMaintenanceReport, ContextMaintenanceTrigger, ContextScope, RunId, RuntimeEvent,
        TaskId, ToolOutput,
    };
    use serde_json::json;

    fn envelope(run: RunId, seq: u64, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: run,
            seq,
            timestamp_ms: seq,
            event,
        }
    }

    fn dummy_report() -> ContextMaintenanceReport {
        ContextMaintenanceReport::default()
    }

    fn tool_output(ok: bool, model_content: &str) -> ToolOutput {
        ToolOutput {
            call_id: "call-1".into(),
            tool_name: "shell.exec".into(),
            ok,
            summary: if ok { "ok" } else { "failed" }.into(),
            model_content: model_content.into(),
            artifact_ref: Some("artifact://run/test.log".into()),
            metadata: json!({}),
        }
    }

    async fn recorded_pin_preview(contents: &[&str]) -> MaterializedContext {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        for content in contents {
            engine
                .ingest(ContextIngress::Pin {
                    content: (*content).into(),
                    kind: ContextKind::Constraint,
                })
                .await
                .unwrap();
        }
        engine
            .ingest(ContextIngress::UserMessage {
                content: "continue".into(),
            })
            .await
            .unwrap();
        engine
            .materialize(ContextQuery {
                current_input: "continue".into(),
                budget_tokens: ReplayConfig::default().budget_tokens,
                hints: ContextHints::default(),
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn replay_answers_lifecycle_questions() {
        let run = RunId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            // The runtime establishes focus (an implicit task) before the
            // first message is ingested.
            envelope(
                run,
                2,
                RuntimeEvent::FocusChanged {
                    task_id: TaskId::new(),
                    goal: "fix AuthService.rs".into(),
                },
            ),
            // Turn 1: user message + one model round.
            envelope(
                run,
                3,
                RuntimeEvent::UserMessageAccepted {
                    content: "fix AuthService.rs".into(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                6,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::BeforeModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                7,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                8,
                RuntimeEvent::AssistantMessage {
                    content: "fixed AuthService".into(),
                },
            ),
            envelope(
                run,
                9,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(run, 10, RuntimeEvent::TurnCompleted),
            // Turn 2: the assistant message from turn 1 becomes prior working
            // context and is consumed by this turn's model request.
            envelope(
                run,
                11,
                RuntimeEvent::UserMessageAccepted {
                    content: "continue".into(),
                },
            ),
            envelope(
                run,
                12,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                13,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                14,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                15,
                RuntimeEvent::AssistantMessage {
                    content: "done".into(),
                },
            ),
            envelope(run, 16, RuntimeEvent::TurnCompleted),
        ];

        let outcome = replay_events(&events, &ReplayConfig::default())
            .await
            .unwrap();

        assert_eq!(outcome.turns, 2);
        assert_eq!(outcome.snapshot_builds, 2);

        // The current turn's user message is excluded from its own working set
        // by design (it is appended as the final user message), so it is never
        // consumed by turn 1 — but as prior context in turn 2 it is consumed.
        let user_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 1)
            .expect("user message item should exist");
        assert_eq!(user_item.scope, ContextScope::Task);
        assert!(!user_item.consumed_turns.contains(&1));
        assert!(
            user_item.consumed_turns.contains(&2),
            "turn-1 user message should be consumed as prior context in turn 2, got {:?}",
            user_item.consumed_turns
        );

        // The assistant message from turn 1 is prior context for turn 2 and
        // must be recorded as consumed by turn 2.
        let assistant_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::AssistantMessage)
            .expect("assistant message item should exist");
        assert_eq!(assistant_item.created_turn, 1);
        assert!(
            assistant_item.consumed_turns.contains(&2),
            "assistant message should be consumed by turn 2, got {:?}",
            assistant_item.consumed_turns
        );

        // The report renders without panicking and mentions both items.
        let report = render_report(&outcome);
        assert!(report.contains("UserMessage"));
        assert!(report.contains("AssistantMessage"));
    }

    #[tokio::test]
    async fn explicit_consumption_replays_only_the_final_recorded_subset() {
        let preview = recorded_pin_preview(&["constraint alpha", "constraint beta"]).await;
        assert_eq!(preview.selected.len(), 2);
        let recorded = preview.selected[1].clone();
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::Pinned {
                    content: "constraint alpha".into(),
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::Pinned {
                    content: "constraint beta".into(),
                },
            ),
            envelope(
                run,
                3,
                RuntimeEvent::UserMessageAccepted {
                    content: "continue".into(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: vec![recorded.clone()],
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ContextConsumed {
                    ack: ContextConsumptionAck {
                        turn_id: TurnId::new(),
                        operation_id: OperationId::new(),
                        model_round: 0,
                        materialization_id: preview.materialization_id,
                        item_ids: vec![recorded.item_id],
                        external_item_ids: Vec::new(),
                    },
                },
            ),
        ];

        let outcome = replay_events(&events, &ReplayConfig::default())
            .await
            .unwrap();
        let constraints: Vec<_> = outcome
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::Constraint)
            .collect();
        assert_eq!(constraints.len(), 2);
        assert_eq!(
            constraints
                .iter()
                .filter(|item| item.consumed_turns == vec![1])
                .count(),
            1,
            "only the final post-packing subset may be recorded as consumed"
        );
        assert_eq!(
            constraints
                .iter()
                .map(|item| item.access_count)
                .sum::<u32>(),
            1,
            "only one replay item may receive access reinforcement"
        );
    }

    #[tokio::test]
    async fn an_unacknowledged_preview_receives_no_replay_reinforcement() {
        let preview = recorded_pin_preview(&["preview only constraint"]).await;
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::Pinned {
                    content: "preview only constraint".into(),
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::UserMessageAccepted {
                    content: "continue".into(),
                },
            ),
            envelope(
                run,
                3,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: preview.selected,
                },
            ),
            // A later empty frame succeeds. It supersedes the first pending
            // preview; the first provider attempt never produced an ack.
            envelope(
                run,
                4,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ContextConsumed {
                    ack: ContextConsumptionAck {
                        turn_id: TurnId::new(),
                        operation_id: OperationId::new(),
                        model_round: 1,
                        materialization_id: 2,
                        item_ids: Vec::new(),
                        external_item_ids: Vec::new(),
                    },
                },
            ),
        ];

        let outcome = replay_events(&events, &ReplayConfig::default())
            .await
            .unwrap();
        let constraint = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Constraint)
            .unwrap();
        assert!(constraint.consumed_turns.is_empty());
        assert_eq!(constraint.access_count, 0);
    }

    #[tokio::test]
    async fn replay_tracks_pin_and_task_completion() {
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::FocusChanged {
                    task_id: TaskId::new(),
                    goal: "task one".into(),
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::UserMessageAccepted {
                    content: "task one".into(),
                },
            ),
            envelope(
                run,
                3,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::Pinned {
                    content: "never edit generated files".into(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::FocusChanged,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                6,
                RuntimeEvent::TaskCompleted {
                    task_id: TaskId::new(),
                    anchor_revision: 0,
                    summary: "task one done".into(),
                },
            ),
            envelope(
                run,
                7,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::TaskCompleted,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                8,
                RuntimeEvent::UserMessageAccepted {
                    content: "task two".into(),
                },
            ),
            envelope(
                run,
                9,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                10,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                11,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                12,
                RuntimeEvent::AssistantMessage {
                    content: "ok".into(),
                },
            ),
            envelope(run, 13, RuntimeEvent::TurnCompleted),
        ];

        let outcome = replay_events(&events, &ReplayConfig::default())
            .await
            .unwrap();
        assert_eq!(outcome.turns, 2);

        let pinned = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Constraint)
            .expect("pinned constraint should exist");
        assert_eq!(pinned.attention, AttentionState::Active, "pinned survives");

        let task_one = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 1)
            .expect("task-one user message should exist");
        assert_eq!(
            task_one.attention,
            AttentionState::Archived,
            "completed task details should be archived"
        );
        let archive = task_one
            .transitions
            .iter()
            .find(|t| t.to == AttentionState::Archived)
            .expect("archival must be recorded as a transition");
        assert!(
            archive.reason.contains("task completed"),
            "unexpected archive reason: {}",
            archive.reason
        );

        let summary_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Summary)
            .expect("task summary should exist");
        assert_eq!(summary_item.created_turn, 1);
    }

    #[tokio::test]
    async fn replay_persists_error_until_verified_then_archives() {
        let run = RunId::new();
        let events = vec![
            // Turn 1: first attempt fails.
            envelope(
                run,
                1,
                RuntimeEvent::UserMessageAccepted {
                    content: "run tests".into(),
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                3,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::BeforeModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ToolFinished {
                    output: tool_output(false, "tests failed: AuthService.rs:42"),
                },
            ),
            envelope(
                run,
                6,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterTool,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                7,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                8,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                9,
                RuntimeEvent::AssistantMessage {
                    content: "fixed the test".into(),
                },
            ),
            envelope(run, 10, RuntimeEvent::TurnCompleted),
            // Turn 2: retry passes on the same entity — the error is verified
            // and archived; the successful observation stays ephemeral.
            envelope(
                run,
                11,
                RuntimeEvent::UserMessageAccepted {
                    content: "retry".into(),
                },
            ),
            envelope(
                run,
                12,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                13,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                14,
                RuntimeEvent::ToolFinished {
                    output: tool_output(true, "tests passed in AuthService.rs"),
                },
            ),
            envelope(
                run,
                15,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterTool,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                16,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                17,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                18,
                RuntimeEvent::AssistantMessage {
                    content: "done".into(),
                },
            ),
            envelope(run, 19, RuntimeEvent::TurnCompleted),
        ];

        let outcome = replay_events(&events, &ReplayConfig::default())
            .await
            .unwrap();
        assert_eq!(outcome.turns, 2);

        // The failed observation is a real item: it survives the model turn
        // (Working retention) and is only archived once the fix is verified.
        let tool_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("failed tool observation should be an Error item");
        assert_eq!(tool_item.created_turn, 1);
        assert_eq!(
            tool_item.attention,
            AttentionState::Archived,
            "verified error should be archived, not dropped"
        );
        let verify = tool_item
            .transitions
            .iter()
            .find(|t| t.reason.contains("verified fixed"))
            .expect("verification must be recorded as a transition");
        assert!(verify.to == AttentionState::Archived);
        assert!(
            tool_item.consumed_turns.contains(&1),
            "observation must be consumed before being archived"
        );

        // The successful observation is ephemeral: consumed after the model
        // turn — leaves attention (Archived), stays semantically live.
        let ok_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("successful observation should exist");
        assert_eq!(
            ok_item.attention,
            AttentionState::Archived,
            "successful observation leaves attention after consumption"
        );
        // Dependency graph: the successful observation shares the
        // AuthService.rs entity with the earlier error, so it must depend on it.
        assert!(
            ok_item.dependencies.contains(&tool_item.id),
            "successful observation must depend on the error it verifies, got {:?}",
            ok_item.dependencies
        );
        assert_eq!(outcome.tool_rounds, 2);
    }

    #[tokio::test]
    async fn replay_file_ignores_other_runs() {
        let run = RunId::new();
        let other = RunId::new();
        let mut events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            envelope(
                run,
                2,
                RuntimeEvent::UserMessageAccepted {
                    content: "hello".into(),
                },
            ),
            envelope(run, 3, RuntimeEvent::TurnCompleted),
        ];
        events.push(envelope(
            other,
            1,
            RuntimeEvent::UserMessageAccepted {
                content: "other run".into(),
            },
        ));

        let jsonl: String = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let path = std::env::temp_dir().join(format!("replay-{}.jsonl", run));
        std::fs::write(&path, jsonl).unwrap();

        let outcome = replay_file(&path, &ReplayConfig::default()).await.unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.items.len(), 1);
    }
}

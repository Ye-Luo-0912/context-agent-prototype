//! Deterministic replay of a context lifecycle from a JSONL event journal.
//!
//! The runtime records every context-relevant action as a `RuntimeEvent`
//! (`UserMessageAccepted`, `Pinned`, `ToolFinished`, `ContextMaintained`,
//! `ContextPrepared`, ...). Replay walks those events in order and drives a
//! fresh context engine with the exact same ingest / maintain /
//! build_snapshot calls, then reports, per item:
//!
//! - what entered and why (`source`, `kind`, entry turn);
//! - which model turns consumed it (from `ContextPrepared` selections);
//! - every state transition, with the maintenance turn and reason;
//! - the final state.
//!
//! The same machinery powers the P3 A/B/C comparison: `scenarios` synthesizes
//! deterministic task traces and `compare_scenario` runs each trace through
//! the append-only, rolling-summary and dynamic-working-set engines to
//! measure input-token cost, over-budget turns and context churn.

mod scenarios;

use std::{collections::HashMap, path::Path, sync::Arc};

use agent_contracts::{
    ContextBuildRequest, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextKind, ContextState, FocusState, RuntimeEvent, RuntimeEventEnvelope,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};

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
    pub from: ContextState,
    pub to: ContextState,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ReplayedItem {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: agent_contracts::ContextScope,
    pub state: ContextState,
    pub source: Option<String>,
    pub created_turn: u64,
    pub access_count: u32,
    /// P4: ids of prior items this item explicitly depends on (shared entities).
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
    /// P3 measurement: total input tokens across all snapshot builds.
    pub input_tokens_total: usize,
    /// P3 measurement: largest single snapshot (worst single model request).
    pub input_tokens_max: usize,
    /// P3 measurement: snapshots that exceeded the configured budget.
    pub over_budget_snapshots: usize,
    /// P3 measurement: total lifecycle transitions emitted by maintenance.
    pub transitions_total: usize,
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
/// replay and the P3 A/B/C comparison.
pub async fn run_engine(
    engine: Arc<dyn ContextEngine>,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<ReplayOutcome> {
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
            RuntimeEvent::FocusChanged { goal } => {
                engine
                    .ingest(ContextIngress::FocusChanged {
                        focus: FocusState::new(goal.clone()),
                    })
                    .await?;
            }
            RuntimeEvent::Pinned { content } => {
                engine
                    .ingest(ContextIngress::Pin {
                        content: content.clone(),
                        kind: ContextKind::Constraint,
                    })
                    .await?;
            }
            RuntimeEvent::TaskCompleted { summary } => {
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
            RuntimeEvent::ContextPrepared { .. } => {
                let snapshot = engine
                    .build_snapshot(ContextBuildRequest {
                        system_prompt: config.system_prompt.clone(),
                        current_input: current_input.clone(),
                        budget_tokens: config.budget_tokens,
                    })
                    .await?;
                snapshot_builds += 1;
                input_tokens_total += snapshot.approx_tokens;
                input_tokens_max = input_tokens_max.max(snapshot.approx_tokens);
                if snapshot.approx_tokens > config.budget_tokens {
                    over_budget_snapshots += 1;
                }
                for selection in snapshot.selected {
                    consumed
                        .entry(selection.item_id)
                        .or_default()
                        .push(current_turn);
                }
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
    })
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
            state: summary.state,
            source: summary.source,
            created_turn: summary.created_turn,
            access_count: summary.access_count,
            dependencies: summary.dependencies,
            consumed_turns: consumed.remove(&summary.id).unwrap_or_default(),
            transitions: transitions.remove(&summary.id).unwrap_or_default(),
        })
        .collect()
}

/// Human-readable lifecycle report answering the P0.5 acceptance questions.
pub fn render_report(outcome: &ReplayOutcome) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "replay: consumed {} events | {} turns | {} tool rounds | {} snapshot builds\n",
        outcome.events_consumed, outcome.turns, outcome.tool_rounds, outcome.snapshot_builds
    ));
    let diagnostics = &outcome.final_diagnostics;
    out.push_str(&format!(
        "final context: total={} active={} cooling={} archived={} dropped={} active~{} tok\n\n",
        diagnostics.total_items,
        diagnostics.active_items,
        diagnostics.cooling_items,
        diagnostics.archived_items,
        diagnostics.dropped_items,
        diagnostics.approx_active_tokens,
    ));

    for item in &outcome.items {
        out.push_str(&format!(
            "item {} [{} | {:?} | {:?}] entered turn {} (source: {})\n",
            short_id(&item.id),
            debug_kind(item.kind),
            item.scope,
            item.state,
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
        out.push_str(&format!("  final state: {:?}\n", item.state));
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
        ToolOutput,
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

    #[tokio::test]
    async fn replay_answers_lifecycle_questions() {
        let run = RunId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            // Turn 1: user message + one model round.
            envelope(
                run,
                2,
                RuntimeEvent::UserMessageAccepted {
                    content: "fix AuthService.rs".into(),
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
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::BeforeModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                6,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                7,
                RuntimeEvent::AssistantMessage {
                    content: "fixed AuthService".into(),
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
            envelope(run, 9, RuntimeEvent::TurnCompleted),
            // Turn 2: the assistant message from turn 1 becomes prior working
            // context and is consumed by this turn's model request.
            envelope(
                run,
                10,
                RuntimeEvent::UserMessageAccepted {
                    content: "continue".into(),
                },
            ),
            envelope(
                run,
                11,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                12,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                13,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                14,
                RuntimeEvent::AssistantMessage {
                    content: "done".into(),
                },
            ),
            envelope(run, 15, RuntimeEvent::TurnCompleted),
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
    async fn replay_tracks_pin_and_task_completion() {
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::UserMessageAccepted {
                    content: "task one".into(),
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
                RuntimeEvent::Pinned {
                    content: "never edit generated files".into(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::FocusChanged,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                5,
                RuntimeEvent::TaskCompleted {
                    summary: "task one done".into(),
                },
            ),
            envelope(
                run,
                6,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::TaskCompleted,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                7,
                RuntimeEvent::UserMessageAccepted {
                    content: "task two".into(),
                },
            ),
            envelope(
                run,
                8,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                9,
                RuntimeEvent::ContextPrepared {
                    diagnostics: ContextDiagnostics::default(),
                    selected: Vec::new(),
                },
            ),
            envelope(
                run,
                10,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: dummy_report(),
                },
            ),
            envelope(
                run,
                11,
                RuntimeEvent::AssistantMessage {
                    content: "ok".into(),
                },
            ),
            envelope(run, 12, RuntimeEvent::TurnCompleted),
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
        assert_eq!(pinned.state, ContextState::Active, "pinned survives");

        let task_one = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage && item.created_turn == 1)
            .expect("task-one user message should exist");
        assert_eq!(
            task_one.state,
            ContextState::Archived,
            "completed task details should be archived"
        );
        let archive = task_one
            .transitions
            .iter()
            .find(|t| t.to == ContextState::Archived)
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
            tool_item.state,
            ContextState::Archived,
            "verified error should be archived, not dropped"
        );
        let verify = tool_item
            .transitions
            .iter()
            .find(|t| t.reason.contains("verified fixed"))
            .expect("verification must be recorded as a transition");
        assert!(verify.to == ContextState::Archived);
        assert!(
            tool_item.consumed_turns.contains(&1),
            "observation must be consumed before being archived"
        );

        // The successful observation is ephemeral: dropped after the model turn.
        let ok_item = outcome
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("successful observation should exist");
        assert_eq!(
            ok_item.state,
            ContextState::Dropped,
            "successful observation should stay ephemeral"
        );
        // P4 dependency graph: the successful observation shares the
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

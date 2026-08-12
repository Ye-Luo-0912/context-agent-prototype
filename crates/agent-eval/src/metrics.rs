//! All-module cost accounting for M15: aggregate every measurable dimension
//! of a run from its event stream without calling a model.
//!
//! The M15 evaluation plan requires per-solved-task accounting of model
//! tokens, tool-schema tokens, lifecycle/GC cost and tool behavior. Some of
//! that is only measurable in the live harness (wall time, provider
//! latency, process launches); everything that is *in the event stream* is
//! aggregated here deterministically, so the accounting logic is tested
//! without a model and reused by the live harness.

use agent_contracts::{RuntimeEvent, RuntimeEventEnvelope};
use std::collections::HashSet;

/// Deterministic per-run measurements aggregated from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetrics {
    // Cost.
    /// Provider-reported input tokens (`ModelUsed`).
    pub model_input_tokens: u64,
    /// Provider-reported output tokens (`ModelUsed`).
    pub model_output_tokens: u64,
    /// Cumulative tool-schema tokens of every round surface
    /// (`ToolSurfacePlanned.selected_schema_tokens`).
    pub schema_tokens_total: u64,
    /// Model rounds that produced a surface plan.
    pub rounds: u64,
    /// User turns (`UserMessageAccepted`).
    pub turns: u64,
    /// Tool calls (`ToolStarted`).
    pub tool_calls: u64,
    /// Lifecycle transitions produced by maintenance passes
    /// (`ContextMaintained.report.transitions`).
    pub lifecycle_transitions: u64,
    /// Full-GC evictions / reactivations / externalizations
    /// (`ContextGc`).
    pub gc_evictions: u64,
    pub gc_reactivations: u64,
    pub gc_externalizations: u64,

    // Tool behavior.
    /// Tool results with `ok: false`.
    pub failed_tool_outputs: u64,
    /// Tool results carrying an artifact reference (spill happened).
    pub artifact_spills: u64,
    /// Cumulative model-facing output chars of tool results.
    pub output_chars_total: u64,
    /// Repeated `fs.read` of the same workspace path (the second and later
    /// reads of one path — a proxy for search/re-read inefficiency).
    pub repeated_fs_reads: u64,

    // Materialization (the Phase 0 measurement baseline: how big the model
    // input actually was and where the working set lived).
    /// `ContextPrepared` events: model rounds that produced a
    /// materialization preview.
    pub materialize_rounds: u64,
    /// Cumulative selected items across previews (`selected.len()`).
    pub selected_items_total: u64,
    /// Cumulative selected-item token estimate (`ContextSelection.approx_tokens`).
    pub selected_tokens_total: u64,
    /// Cumulative `approx_active_tokens` across previews — the engine's
    /// estimate of the model-visible working set per round.
    pub active_tokens_total: u64,
    /// Final materialization diagnostics: the residency split the last
    /// preview saw (resident heap / warm buffer / cold store / external).
    pub final_total_items: u64,
    pub final_resident_items: u64,
    pub final_warm_items: u64,
    pub final_cold_items: u64,
    pub final_external_items: u64,
    /// Cumulative store I/O from full-GC passes (`ContextGc`): bytes
    /// written (externalization), bytes read back (recall), and items
    /// recalled from the store.
    pub store_write_bytes_total: u64,
    pub store_read_bytes_total: u64,
    pub store_recalled_items_total: u64,
    /// Materialization latency percentiles (ms) across `ContextPrepared`
    /// previews (the engine's own materialize call, before runtime
    /// rendering overhead). 0 when no preview carried a timestamp.
    pub materialize_ms_p50: u64,
    pub materialize_ms_p95: u64,
}

/// Aggregate one run's envelopes. The caller filters to a single run.
pub fn aggregate_metrics(events: &[RuntimeEventEnvelope]) -> RunMetrics {
    let mut metrics = RunMetrics::default();
    let mut read_paths: HashSet<String> = HashSet::new();
    let mut materialize_ms_samples: Vec<u64> = Vec::new();

    for envelope in events {
        match &envelope.event {
            RuntimeEvent::UserMessageAccepted { .. } => metrics.turns += 1,
            RuntimeEvent::ToolStarted { call } => {
                metrics.tool_calls += 1;
                if call.name == "fs.read"
                    && let Some(path) = call.arguments.get("path").and_then(|value| value.as_str())
                    && !read_paths.insert(path.to_string())
                {
                    metrics.repeated_fs_reads += 1;
                }
            }
            RuntimeEvent::ToolFinished { output } => {
                if !output.ok {
                    metrics.failed_tool_outputs += 1;
                }
                if output.artifact_ref.is_some() {
                    metrics.artifact_spills += 1;
                }
                metrics.output_chars_total += output.model_content.chars().count() as u64;
            }
            RuntimeEvent::ContextMaintained { report, .. } => {
                metrics.lifecycle_transitions += report.transitions.len() as u64;
            }
            RuntimeEvent::ContextGc { report } => {
                metrics.gc_evictions += report.evicted as u64;
                metrics.gc_reactivations += report.reactivated as u64;
                metrics.gc_externalizations += report.externalized as u64;
                metrics.store_write_bytes_total += report.store_write_bytes;
                metrics.store_read_bytes_total += report.store_read_bytes;
                metrics.store_recalled_items_total += report.store_recalled_items;
            }
            RuntimeEvent::ToolSurfacePlanned { report } => {
                metrics.rounds += 1;
                metrics.schema_tokens_total += report.selected_schema_tokens as u64;
            }
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
            } => {
                metrics.model_input_tokens += input_tokens;
                metrics.model_output_tokens += output_tokens;
            }
            RuntimeEvent::ContextPrepared {
                diagnostics,
                selected,
                materialize_ms,
            } => {
                metrics.materialize_rounds += 1;
                metrics.selected_items_total += selected.len() as u64;
                metrics.selected_tokens_total += selected
                    .iter()
                    .map(|item| item.approx_tokens as u64)
                    .sum::<u64>();
                metrics.active_tokens_total += diagnostics.approx_active_tokens as u64;
                if *materialize_ms > 0 {
                    materialize_ms_samples.push(*materialize_ms);
                }
                // The last preview's diagnostics are the run's final
                // residency snapshot (Resident/Warm/Cold/External counts).
                metrics.final_total_items = diagnostics.total_items as u64;
                metrics.final_resident_items = diagnostics.resident_items as u64;
                metrics.final_warm_items = diagnostics.warm_items as u64;
                metrics.final_cold_items = diagnostics.cold_items as u64;
                metrics.final_external_items = diagnostics.external_items as u64;
            }
            _ => {}
        }
    }
    if !materialize_ms_samples.is_empty() {
        materialize_ms_samples.sort_unstable();
        metrics.materialize_ms_p50 = percentile(&materialize_ms_samples, 50);
        metrics.materialize_ms_p95 = percentile(&materialize_ms_samples, 95);
    }
    metrics
}

/// The nearest-rank percentile of a sorted sample: index
/// `round((len - 1) * pct / 100)`.
fn percentile(sorted: &[u64], pct: u64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let index = ((sorted.len() - 1) as f64 * pct as f64 / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Human-readable metric block for one run.
pub fn render_metrics(metrics: &RunMetrics) -> String {
    format!(
        "cost: model_in={} model_out={} schema_tokens={} rounds={} turns={} lifecycle_transitions={}\n\
         gc: evictions={} reactivations={} externalizations={}\n\
         materialize: rounds={} selected_items={} selected_tokens={} active_tokens={}\n\
         materialize_latency: p50={}ms p95={}ms\n\
         residency(final): total={} resident={} warm={} cold={} external={}\n\
         store: write_bytes={} read_bytes={} recalled_items={}\n\
         behavior: tool_calls={} failed_outputs={} spills={} output_chars={} repeated_fs_reads={}\n",
        metrics.model_input_tokens,
        metrics.model_output_tokens,
        metrics.schema_tokens_total,
        metrics.rounds,
        metrics.turns,
        metrics.lifecycle_transitions,
        metrics.gc_evictions,
        metrics.gc_reactivations,
        metrics.gc_externalizations,
        metrics.materialize_rounds,
        metrics.selected_items_total,
        metrics.selected_tokens_total,
        metrics.active_tokens_total,
        metrics.materialize_ms_p50,
        metrics.materialize_ms_p95,
        metrics.final_total_items,
        metrics.final_resident_items,
        metrics.final_warm_items,
        metrics.final_cold_items,
        metrics.final_external_items,
        metrics.store_write_bytes_total,
        metrics.store_read_bytes_total,
        metrics.store_recalled_items_total,
        metrics.tool_calls,
        metrics.failed_tool_outputs,
        metrics.artifact_spills,
        metrics.output_chars_total,
        metrics.repeated_fs_reads,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AttentionState, ContextDiagnostics, ContextGcReport, ContextItemId, ContextKind,
        ContextMaintenanceReport, ContextMaintenanceTrigger, ContextScope, ContextSelection,
        ContextStateTransition, RunId, ScoreBreakdown, TaskId, ToolOutput, ToolSurfaceDemand,
        ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceSelection,
        ToolSurfaceSourceRevisions, TurnId,
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

    fn transition() -> ContextStateTransition {
        ContextStateTransition {
            item_id: ContextItemId::new(),
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            from: AttentionState::Active,
            to: AttentionState::Archived,
            turn: 1,
            reason: "test".into(),
        }
    }

    #[test]
    fn aggregates_every_measurable_dimension() {
        let run = RunId::new();
        let mut seq = 1;
        let mut events = Vec::new();

        events.push(envelope(run, seq, RuntimeEvent::RunStarted));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "task".into(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::UserMessageAccepted {
                content: "fix it".into(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolStarted {
                call: agent_contracts::ToolCall {
                    id: "1".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/main.rs"}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "content".into(),
                    artifact_ref: Some("artifact://run/full.txt".into()),
                    metadata: json!({}),
                },
            },
        ));
        seq += 1;
        // A repeated read of the same path counts.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolStarted {
                call: agent_contracts::ToolCall {
                    id: "2".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/main.rs"}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "2".into(),
                    tool_name: "fs.read".into(),
                    ok: false,
                    summary: "failed".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::UserInput,
                report: ContextMaintenanceReport {
                    promoted: 1,
                    cooled: 2,
                    archived: 1,
                    tombstoned: 0,
                    turn: 1,
                    transitions: vec![transition(), transition()],
                    diagnostics: Default::default(),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextGc {
                report: ContextGcReport {
                    marked_roots: 3,
                    resident: 5,
                    evicted: 2,
                    reactivated: 1,
                    externalized: 1,
                    aged_external: 0,
                    evictions: Vec::new(),
                    reactivations: Vec::new(),
                    store_blob_delete_errors: 0,
                    store_write_bytes: 512,
                    store_read_bytes: 128,
                    store_recalled_items: 2,
                    diagnostics: Default::default(),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolSurfacePlanned {
                report: ToolSurfacePlanReport {
                    turn_id: TurnId::new(),
                    model_round: 1,
                    surface_revision: 1,
                    source_revisions: ToolSurfaceSourceRevisions::default(),
                    status: ToolSurfacePlanStatus::Ready,
                    selected: vec![ToolSurfaceSelection {
                        tool_name: "fs.read".into(),
                        demand: ToolSurfaceDemand::MustSurface,
                        origin: agent_contracts::ToolSurfaceOrigin::DispatcherRequired,
                        approx_tokens: 100,
                    }],
                    selected_total: 1,
                    omitted: Vec::new(),
                    omitted_total: 0,
                    blocked: Vec::new(),
                    blocked_total: 0,
                    selected_schema_tokens: 512,
                    mandatory_schema_tokens: 512,
                    estimated_input_tokens: 10_000,
                    input_budget_tokens: 24_000,
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ModelUsed {
                input_tokens: 9_000,
                output_tokens: 120,
            },
        ));
        seq += 1;
        // Two materialization previews: cumulative sums add up, the final
        // residency snapshot is the last preview's, and the latency
        // percentiles come from the timestamped samples.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextPrepared {
                diagnostics: ContextDiagnostics {
                    total_items: 10,
                    resident_items: 6,
                    warm_items: 2,
                    cold_items: 1,
                    external_items: 1,
                    approx_active_tokens: 5_000,
                    ..ContextDiagnostics::default()
                },
                selected: vec![
                    ContextSelection {
                        item_id: ContextItemId::new(),
                        score: 1.0,
                        approx_tokens: 300,
                        reason: "focus".into(),
                        breakdown: ScoreBreakdown::default(),
                    },
                    ContextSelection {
                        item_id: ContextItemId::new(),
                        score: 0.5,
                        approx_tokens: 200,
                        reason: "recall".into(),
                        breakdown: ScoreBreakdown::default(),
                    },
                ],
                materialize_ms: 10,
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextPrepared {
                diagnostics: ContextDiagnostics {
                    total_items: 11,
                    resident_items: 7,
                    warm_items: 2,
                    cold_items: 1,
                    external_items: 1,
                    approx_active_tokens: 4_000,
                    ..ContextDiagnostics::default()
                },
                selected: vec![ContextSelection {
                    item_id: ContextItemId::new(),
                    score: 0.9,
                    approx_tokens: 250,
                    reason: "focus".into(),
                    breakdown: ScoreBreakdown::default(),
                }],
                materialize_ms: 20,
            },
        ));
        seq += 1;
        events.push(envelope(run, seq, RuntimeEvent::TurnCompleted));

        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.turns, 1);
        assert_eq!(metrics.tool_calls, 2);
        assert_eq!(metrics.repeated_fs_reads, 1);
        assert_eq!(metrics.failed_tool_outputs, 1);
        assert_eq!(metrics.artifact_spills, 1);
        assert_eq!(metrics.output_chars_total, 7, "content + failed(0)");
        assert_eq!(metrics.lifecycle_transitions, 2);
        assert_eq!(metrics.gc_evictions, 2);
        assert_eq!(metrics.gc_reactivations, 1);
        assert_eq!(metrics.gc_externalizations, 1);
        assert_eq!(metrics.rounds, 1);
        assert_eq!(metrics.schema_tokens_total, 512);
        assert_eq!(metrics.model_input_tokens, 9_000);
        assert_eq!(metrics.model_output_tokens, 120);
        assert_eq!(metrics.materialize_rounds, 2);
        assert_eq!(metrics.selected_items_total, 3);
        assert_eq!(metrics.selected_tokens_total, 750);
        assert_eq!(metrics.active_tokens_total, 9_000);
        assert_eq!(metrics.final_total_items, 11);
        assert_eq!(metrics.final_resident_items, 7);
        assert_eq!(metrics.final_warm_items, 2);
        assert_eq!(metrics.final_cold_items, 1);
        assert_eq!(metrics.final_external_items, 1);
        assert_eq!(metrics.store_write_bytes_total, 512);
        assert_eq!(metrics.store_read_bytes_total, 128);
        assert_eq!(metrics.store_recalled_items_total, 2);
        // Two samples [10, 20]: the nearest-rank p50/p95 both land on the
        // second sample.
        assert_eq!(metrics.materialize_ms_p50, 20);
        assert_eq!(metrics.materialize_ms_p95, 20);

        let rendered = render_metrics(&metrics);
        assert!(rendered.contains("model_in=9000"));
        assert!(rendered.contains("repeated_fs_reads=1"));
        assert!(rendered.contains("materialize: rounds=2 selected_items=3 selected_tokens=750"));
        assert!(
            rendered.contains("residency(final): total=11 resident=7 warm=2 cold=1 external=1")
        );
        assert!(rendered.contains("materialize_latency: p50=20ms p95=20ms"));
        assert!(rendered.contains("store: write_bytes=512 read_bytes=128 recalled_items=2"));
    }

    #[test]
    fn materialize_percentiles_use_nearest_rank() {
        // Four samples [10, 20, 30, 40]: p50 = round(1.5) -> index 2 (30),
        // p95 = round(2.85) -> index 3 (40).
        let sorted = vec![10u64, 20, 30, 40];
        assert_eq!(percentile(&sorted, 50), 30);
        assert_eq!(percentile(&sorted, 95), 40);

        // A single sample is both p50 and p95.
        assert_eq!(percentile(&[7u64], 50), 7);
        assert_eq!(percentile(&[7u64], 95), 7);
    }
}

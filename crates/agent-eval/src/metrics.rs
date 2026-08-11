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
}

/// Aggregate one run's envelopes. The caller filters to a single run.
pub fn aggregate_metrics(events: &[RuntimeEventEnvelope]) -> RunMetrics {
    let mut metrics = RunMetrics::default();
    let mut read_paths: HashSet<String> = HashSet::new();

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
            _ => {}
        }
    }
    metrics
}

/// Human-readable metric block for one run.
pub fn render_metrics(metrics: &RunMetrics) -> String {
    format!(
        "cost: model_in={} model_out={} schema_tokens={} rounds={} turns={} lifecycle_transitions={}\n\
         gc: evictions={} reactivations={} externalizations={}\n\
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
        AttentionState, ContextGcReport, ContextItemId, ContextKind, ContextMaintenanceReport,
        ContextMaintenanceTrigger, ContextScope, ContextStateTransition, RunId, TaskId, ToolOutput,
        ToolSurfaceDemand, ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceSelection,
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

        let rendered = render_metrics(&metrics);
        assert!(rendered.contains("model_in=9000"));
        assert!(rendered.contains("repeated_fs_reads=1"));
    }
}

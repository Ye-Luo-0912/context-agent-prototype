//! Evidence frontier reconstruction from a JSONL event trace.
//!
//! The frontier answers "what did this run actually prove, and where did
//! it stall". Reconstruction drives the runtime's own
//! [`ExecutionState::observe_tool`] projection over the trace's
//! `ToolFinished` outputs, so replay and runtime share one deterministic
//! classification — no duplicated semantics, no drift.
//!
//! Scope honesty: journaled `ToolFinished` rows carry no disposition or
//! model-round number, so the rebuild uses a synthetic turn counter for
//! recency only. Anchor-relative views (verification rows) are therefore
//! not reconstructed here; the frontier rows and convergence counters are.

use agent_contracts::{FrontierDelta, RuntimeEvent, RuntimeEventEnvelope};
use agent_runtime::ExecutionState;

/// The rebuilt evidence frontier of one run: final typed evidence rows
/// plus the convergence counters aggregated along the way. All fields are
/// bounded like the live state they mirror.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrontierRebuild {
    /// Final typed evidence rows (newest first), rendered by the same
    /// projection the prompt uses.
    pub operational_evidence: Vec<String>,
    /// Persistable tool observations classified.
    pub observations: u64,
    /// Observations whose delta provably advanced the frontier
    /// (world change / evidence gain / obligation resolved).
    pub advances: u64,
    /// Same-revision repeats of known evidence.
    pub redundant: u64,
    /// Same semantic evidence that only repaired currentness after an
    /// invalidation.
    pub reconfirmed: u64,
    /// Evidence rows invalidated by world-revision advances.
    pub invalidations: u64,
    /// Peak of consecutive non-advance actions (advisory fires at 5).
    pub no_advance_peak: u32,
    /// Final monotonic evidence revision of the rebuilt state.
    pub evidence_revision: u64,
}

/// Rebuild the evidence frontier by re-driving the runtime projection
/// over every `ToolFinished` output in the trace, in journal order.
pub fn rebuild_frontier(envelopes: &[RuntimeEventEnvelope]) -> FrontierRebuild {
    let mut state = ExecutionState::default();
    let mut rebuild = FrontierRebuild::default();
    for envelope in envelopes {
        let RuntimeEvent::ToolFinished { output } = &envelope.event else {
            continue;
        };
        // 每条入账的 ToolFinished 都是一次持久化观察；轮号仅用于
        // 新近度排序。
        rebuild.observations += 1;
        let observation = state.observe_tool(output, 0, rebuild.observations);
        if observation.delta.advances_frontier() {
            rebuild.advances += 1;
        }
        if observation.delta == FrontierDelta::RedundantEvidence {
            rebuild.redundant += 1;
        }
        if observation.delta == FrontierDelta::EvidenceReconfirmed {
            rebuild.reconfirmed += 1;
        }
        rebuild.invalidations += observation.invalidated;
        rebuild.no_advance_peak = rebuild
            .no_advance_peak
            .max(observation.actions_since_frontier_advance);
    }
    rebuild.evidence_revision = state.convergence.evidence_revision;
    rebuild.operational_evidence = state.view().operational_evidence;
    rebuild
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{RunId, ToolOutput};
    use serde_json::json;

    fn envelope(event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: RunId::new(),
            seq: 1,
            timestamp_ms: 0,
            event,
        }
    }

    fn git_status() -> ToolOutput {
        ToolOutput {
            call_id: "c".into(),
            tool_name: "git.status".into(),
            ok: true,
            summary: "on branch main, clean".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: json!({}),
        }
    }

    #[test]
    fn repeat_observations_show_up_as_redundant_not_advances() {
        let trace = vec![
            envelope(RuntimeEvent::ToolFinished {
                output: git_status(),
            }),
            envelope(RuntimeEvent::ToolFinished {
                output: git_status(),
            }),
        ];
        let rebuild = rebuild_frontier(&trace);
        assert_eq!(rebuild.observations, 2);
        assert_eq!(rebuild.advances, 1, "only the first status is new");
        assert_eq!(rebuild.redundant, 1);
        assert_eq!(rebuild.no_advance_peak, 1);
        assert_eq!(rebuild.operational_evidence.len(), 1);
        assert!(rebuild.operational_evidence[0].starts_with("git.status: "));
    }

    #[test]
    fn non_tool_events_are_ignored() {
        let trace = vec![envelope(RuntimeEvent::TurnCompleted)];
        let rebuild = rebuild_frontier(&trace);
        assert_eq!(rebuild, FrontierRebuild::default());
    }
}

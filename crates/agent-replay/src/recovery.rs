//! Crash-recovery replay: re-read a JSONL event trace to rebuild the
//! context-engine state and locate the durability barrier after a failed
//! turn commit.
//!
//! The runtime's `TurnCompleted` barrier is the durability contract: events
//! at or before the last successful `TurnCompleted` are durably committed,
//! and a failed barrier (`TurnCommitFailed` + `RecoveryRequired`) means the
//! turn did *not* commit — the runtime drops the turn frame and fences
//! further mutation until a known-good restore. This module answers the
//! recovery question from the trace alone:
//!
//! - where the last committed barrier stands and why (phase/message of the
//!   failure that forced recovery, if any);
//! - whether the envelope sequence is contiguous (a gap means events were
//!   lost or duplicated on disk);
//! - the context-engine state rebuilt from the trace (fresh engine, full
//!   deterministic replay) — the "truth" a recovery can trust;
//! - optionally, a restore-consistency proof: a context checkpoint restored
//!   and then advanced by the events after it must equal the full rebuild,
//!   which is the engine-level guarantee that the runtime and the context
//!   never drift apart after a crash recovery.
//!
//! Scope honesty: the trace is an audit stream, not a state-replay log. It
//! carries every context-relevant operation (ingest/maintain/materialize/
//! GC/consumption) so the *context plane* can be rebuilt deterministically;
//! the runtime's `TaskManager` detail (anchor content, requirement
//! revisions) is checkpoint-only and is not reconstructed here.

use std::path::Path;

use agent_contracts::{
    ContextDiagnostics, ContextEngine, RunId, RuntimeEvent, RuntimeEventEnvelope,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::Value;

use crate::{ReplayConfig, ReplayOutcome, run_engine_observing};

/// Where the trace's durability barrier stands and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBarrier {
    /// seq of the last event covered by a successful `TurnCompleted`
    /// barrier. Events at or before this seq are durably committed; a
    /// crash-recovery rebuild may treat everything before it as trusted.
    /// `0` means no turn ever committed (the crash happened before the
    /// first barrier).
    pub last_committed_seq: u64,
    /// The turn-commit failure that demanded recovery, when the trace
    /// shows one. The runtime stops committing after the first failure
    /// and drops the turn frame, so there is at most one.
    pub failure: Option<RecoveryFailure>,
    /// Events after the failure point. The runtime fences mutation after
    /// `RecoveryRequired`, so this should be empty or terminal noise
    /// (warnings/errors) only; a large count is itself a red flag.
    pub events_after_failure: usize,
}

/// One failed turn commit: the exact step recovery must look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryFailure {
    pub seq: u64,
    pub phase: String,
    pub message: String,
}

/// Result of re-reading one trace for crash recovery.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub run_id: RunId,
    /// Number of envelopes belonging to the trace's run.
    pub envelopes: usize,
    /// Whether the envelope seq is contiguous starting at 1.
    pub seq_contiguous: bool,
    /// The first gap found, as `(expected_seq, found_seq)`.
    pub seq_gap: Option<(u64, u64)>,
    pub barrier: RecoveryBarrier,
    /// The context-engine state rebuilt from the trace: a fresh engine
    /// replaying every event of the run in order. Deterministic — same
    /// trace, same engine version, same rebuilt state.
    pub rebuilt: ReplayOutcome,
    /// Final diagnostics of the rebuilt engine (the "truth" state).
    pub rebuilt_diagnostics: ContextDiagnostics,
}

/// Proof that restoring a context checkpoint and then replaying the events
/// after it reproduces the full rebuild from the trace. This is the
/// engine-level half of "the runtime and the context never drift into a
/// task/state split-brain": whatever the checkpoint captured, the trace
/// agrees with it.
#[derive(Debug, Clone)]
pub struct RestoreConsistencyReport {
    /// Diagnostics after restoring `checkpoint` and replaying every event
    /// with `seq > checkpoint_cover_seq`.
    pub incremental_diagnostics: ContextDiagnostics,
    /// Diagnostics of a fresh full replay of the whole run.
    pub full_diagnostics: ContextDiagnostics,
    /// Whether the two agree on every compared dimension.
    pub consistent: bool,
    /// The first dimension that disagreed, when inconsistent.
    pub first_difference: Option<String>,
}

/// Analyze the durability barrier of one run's envelopes.
pub fn analyze_barrier(events: &[RuntimeEventEnvelope]) -> RecoveryBarrier {
    let mut last_committed_seq = 0u64;
    let mut failure: Option<RecoveryFailure> = None;
    for envelope in events {
        match &envelope.event {
            RuntimeEvent::TurnCompleted => last_committed_seq = envelope.seq,
            RuntimeEvent::TurnCommitFailed { phase, message } if failure.is_none() => {
                failure = Some(RecoveryFailure {
                    seq: envelope.seq,
                    phase: phase.clone(),
                    message: message.clone(),
                });
            }
            _ => {}
        }
    }
    let events_after_failure = failure
        .as_ref()
        .map(|failed| {
            events
                .iter()
                .filter(|envelope| envelope.seq > failed.seq)
                .count()
        })
        .unwrap_or(0);
    RecoveryBarrier {
        last_committed_seq,
        failure,
        events_after_failure,
    }
}

/// Check the envelope sequence is contiguous from 1 (first run filter is the
/// caller's responsibility; a single run's seq starts at 1 and increases by
/// one per event).
pub fn first_seq_gap(events: &[RuntimeEventEnvelope]) -> Option<(u64, u64)> {
    for (expected, envelope) in (1u64..).zip(events.iter()) {
        if envelope.seq != expected {
            return Some((expected, envelope.seq));
        }
    }
    None
}

/// Rebuild the context-engine state from one run's envelopes: a fresh engine
/// replays every event deterministically. Returns the measurement outcome and
/// the final diagnostics of that engine.
pub async fn rebuild_engine_state(
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<(ReplayOutcome, ContextDiagnostics)> {
    let engine: std::sync::Arc<dyn ContextEngine> =
        std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let outcome = run_engine_observing(engine.clone(), events, config, |_, _| {}).await?;
    let diagnostics = engine.diagnostics().await?;
    Ok((outcome, diagnostics))
}

/// Read one trace file, keep the first run's envelopes, and produce the
/// crash-recovery report: barrier location, sequence integrity, and the
/// rebuilt context state.
pub async fn recovery_replay_file(
    path: &Path,
    config: &ReplayConfig,
) -> anyhow::Result<RecoveryReport> {
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| anyhow::anyhow!("read trace {}: {error}", path.display()))?;

    let mut envelopes: Vec<RuntimeEventEnvelope> = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let envelope: RuntimeEventEnvelope = serde_json::from_str(line)
            .map_err(|error| anyhow::anyhow!("parse trace line {}: {error}", index + 1))?;
        envelopes.push(envelope);
    }

    let Some(first) = envelopes.first() else {
        anyhow::bail!("trace {} contains no events", path.display());
    };
    let run_id = first.run_id;
    envelopes.retain(|envelope| envelope.run_id == run_id);

    recovery_replay(&envelopes, config).await
}

/// Produce the crash-recovery report for one run's envelopes.
pub async fn recovery_replay(
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<RecoveryReport> {
    let run_id = events
        .first()
        .map(|envelope| envelope.run_id)
        .ok_or_else(|| anyhow::anyhow!("recovery replay needs at least one event"))?;
    let barrier = analyze_barrier(events);
    let seq_gap = first_seq_gap(events);
    let (rebuilt, rebuilt_diagnostics) = rebuild_engine_state(events, config).await?;
    Ok(RecoveryReport {
        run_id,
        envelopes: events.len(),
        seq_contiguous: seq_gap.is_none(),
        seq_gap,
        barrier,
        rebuilt,
        rebuilt_diagnostics,
    })
}

/// Prove a context checkpoint agrees with the trace: restore it into a fresh
/// engine, replay the events after `checkpoint_cover_seq`, and compare the
/// resulting diagnostics with a full rebuild from the trace.
///
/// `checkpoint_cover_seq` is the seq of the last event the checkpoint was
/// captured after — the caller knows it because a checkpoint is captured at
/// a runtime safe point (the runtime captures at turn boundaries, so the
/// natural value is the last committed `TurnCompleted` seq). The checkpoint
/// must be captured at a complete preview/consumption boundary, otherwise
/// the incremental replay may encounter an unmatched acknowledgement.
pub async fn verify_restore_consistency(
    checkpoint: &Value,
    checkpoint_cover_seq: u64,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
) -> anyhow::Result<RestoreConsistencyReport> {
    // Full rebuild: fresh engine replaying every event.
    let (_, full_diagnostics) = rebuild_engine_state(events, config).await?;

    // Incremental: restore the checkpoint, then replay only the events that
    // landed after it. The engine's own `restore` is a whole-state replace,
    // so the replayed tail applies on exactly the captured state.
    let engine: std::sync::Arc<dyn ContextEngine> =
        std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    engine.restore(checkpoint.clone()).await?;
    let tail: Vec<RuntimeEventEnvelope> = events
        .iter()
        .filter(|envelope| envelope.seq > checkpoint_cover_seq)
        .cloned()
        .collect();
    run_engine_observing(engine.clone(), &tail, config, |_, _| {}).await?;
    let incremental_diagnostics = engine.diagnostics().await?;

    let (consistent, first_difference) =
        compare_diagnostics(&incremental_diagnostics, &full_diagnostics);

    Ok(RestoreConsistencyReport {
        incremental_diagnostics,
        full_diagnostics,
        consistent,
        first_difference,
    })
}

/// Compare every dimension of two diagnostics snapshots, returning the first
/// mismatched dimension name when they differ.
fn compare_diagnostics(
    left: &ContextDiagnostics,
    right: &ContextDiagnostics,
) -> (bool, Option<String>) {
    let pairs: [(&str, usize, usize); 17] = [
        ("total_items", left.total_items, right.total_items),
        ("active_items", left.active_items, right.active_items),
        ("cooling_items", left.cooling_items, right.cooling_items),
        ("archived_items", left.archived_items, right.archived_items),
        (
            "tombstoned_items",
            left.tombstoned_items,
            right.tombstoned_items,
        ),
        ("resident_items", left.resident_items, right.resident_items),
        ("warm_items", left.warm_items, right.warm_items),
        ("cold_items", left.cold_items, right.cold_items),
        ("external_items", left.external_items, right.external_items),
        (
            "focus_generation",
            left.focus_generation as usize,
            right.focus_generation as usize,
        ),
        ("turn", left.turn as usize, right.turn as usize),
        (
            "event_seq",
            left.event_seq as usize,
            right.event_seq as usize,
        ),
        (
            "tool_round",
            left.tool_round as usize,
            right.tool_round as usize,
        ),
        ("open_scopes", left.open_scopes, right.open_scopes),
        ("active_scopes", left.active_scopes, right.active_scopes),
        (
            "suspended_scopes",
            left.suspended_scopes,
            right.suspended_scopes,
        ),
        ("closed_scopes", left.closed_scopes, right.closed_scopes),
    ];
    for (name, left_value, right_value) in pairs {
        if left_value != right_value {
            return (false, Some(name.to_string()));
        }
    }
    let u64_pairs: [(&str, u64, u64); 4] = [
        (
            "gc_evicted_total",
            left.gc_evicted_total,
            right.gc_evicted_total,
        ),
        (
            "gc_reactivated_total",
            left.gc_reactivated_total,
            right.gc_reactivated_total,
        ),
        (
            "gc_externalized_total",
            left.gc_externalized_total,
            right.gc_externalized_total,
        ),
        (
            "gc_storage_deleted_total",
            left.gc_storage_deleted_total,
            right.gc_storage_deleted_total,
        ),
    ];
    for (name, left_value, right_value) in u64_pairs {
        if left_value != right_value {
            return (false, Some(name.to_string()));
        }
    }
    (true, None)
}

/// Human-readable crash-recovery report.
pub fn render_recovery_report(report: &RecoveryReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "recovery replay: run {} | {} events | seq contiguous: {}\n",
        report.run_id,
        report.envelopes,
        if report.seq_contiguous { "yes" } else { "no" }
    ));
    if let Some((expected, found)) = report.seq_gap {
        out.push_str(&format!(
            "  seq gap: expected {expected}, found {found} — events were lost or duplicated on disk\n"
        ));
    }
    let barrier = &report.barrier;
    out.push_str(&format!(
        "durability barrier: last committed seq = {}\n",
        barrier.last_committed_seq
    ));
    match &barrier.failure {
        Some(failure) => {
            out.push_str(&format!(
                "  turn commit FAILED at seq {} (phase: {}): {}\n",
                failure.seq, failure.phase, failure.message
            ));
            out.push_str(&format!(
                "  events after the failure: {} (runtime fences mutation after RecoveryRequired)\n",
                barrier.events_after_failure
            ));
        }
        None => out.push_str("  no turn-commit failure in this trace\n"),
    }
    let diagnostics = &report.rebuilt_diagnostics;
    out.push_str(&format!(
        "rebuilt context: total={} active={} cooling={} archived={} resident={} warm={} cold={} external={} | turn={} event_seq={}\n",
        diagnostics.total_items,
        diagnostics.active_items,
        diagnostics.cooling_items,
        diagnostics.archived_items,
        diagnostics.resident_items,
        diagnostics.warm_items,
        diagnostics.cold_items,
        diagnostics.external_items,
        diagnostics.turn,
        diagnostics.event_seq,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ContextConsumptionAck, ContextMaintenanceReport, ContextMaintenanceTrigger,
        ContextSelection, OperationId, TaskId, ToolOutput, TurnId,
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

    fn prepared_event(run: RunId, seq: u64) -> RuntimeEventEnvelope {
        envelope(
            run,
            seq,
            RuntimeEvent::ContextPrepared {
                diagnostics: ContextDiagnostics::default(),
                selected: Vec::<ContextSelection>::new(),
            },
        )
    }

    fn consumed_event(run: RunId, seq: u64, model_round: usize) -> RuntimeEventEnvelope {
        envelope(
            run,
            seq,
            RuntimeEvent::ContextConsumed {
                ack: ContextConsumptionAck {
                    turn_id: TurnId::new(),
                    operation_id: OperationId::new(),
                    model_round,
                    materialization_id: model_round as u64,
                    item_ids: Vec::new(),
                    external_item_ids: Vec::new(),
                },
            },
        )
    }

    /// A two-turn happy-path trace with explicit consumption acknowledgements
    /// and two successful `TurnCompleted` barriers.
    fn happy_trace(run: RunId) -> Vec<RuntimeEventEnvelope> {
        let mut events = vec![envelope(run, 1, RuntimeEvent::RunStarted)];
        let mut seq = 2;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "fix AuthService.rs".into(),
            },
        ));
        seq += 1;
        // Turn 1.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::UserMessageAccepted {
                content: "fix AuthService.rs".into(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::UserInput,
                report: dummy_report(),
            },
        ));
        seq += 1;
        events.push(prepared_event(run, seq));
        seq += 1;
        events.push(consumed_event(run, seq, 1));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: tool_output(true, "found AuthService.rs"),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::AfterModel,
                report: dummy_report(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::AssistantMessage {
                content: "fixing".into(),
            },
        ));
        seq += 1;
        events.push(envelope(run, seq, RuntimeEvent::TurnCompleted));
        seq += 1;
        // Turn 2.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::UserMessageAccepted {
                content: "verify".into(),
            },
        ));
        seq += 1;
        events.push(prepared_event(run, seq));
        seq += 1;
        events.push(consumed_event(run, seq, 2));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::AssistantMessage {
                content: "done".into(),
            },
        ));
        seq += 1;
        events.push(envelope(run, seq, RuntimeEvent::TurnCompleted));
        events
    }

    #[tokio::test]
    async fn recovery_locates_the_last_committed_barrier() {
        let run = RunId::new();
        let events = happy_trace(run);
        let report = recovery_replay(&events, &ReplayConfig::default())
            .await
            .unwrap();

        assert_eq!(report.run_id, run);
        assert!(report.seq_contiguous);
        assert_eq!(report.seq_gap, None);
        assert_eq!(report.barrier.last_committed_seq, events.len() as u64);
        assert_eq!(report.barrier.failure, None);
        assert_eq!(report.barrier.events_after_failure, 0);
        assert_eq!(report.rebuilt.turns, 2);
        assert_eq!(report.rebuilt.snapshot_builds, 2);
    }

    #[tokio::test]
    async fn recovery_reports_the_turn_commit_failure_and_fences_after() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        let last = events.last().unwrap().seq;
        events.push(envelope(
            run,
            last + 1,
            RuntimeEvent::UserMessageAccepted {
                content: "third turn".into(),
            },
        ));
        let failed_seq = last + 2;
        events.push(envelope(
            run,
            failed_seq,
            RuntimeEvent::TurnCommitFailed {
                phase: "turn_completed_event".into(),
                message: "journal flush failed: storage error".into(),
            },
        ));
        // After the failure the runtime fences mutation, but the trace may
        // still carry a terminal warning.
        events.push(envelope(
            run,
            failed_seq + 1,
            RuntimeEvent::RecoveryRequired,
        ));
        events.push(envelope(
            run,
            failed_seq + 2,
            RuntimeEvent::Warning {
                message: "mutation fenced until restore".into(),
            },
        ));

        let report = recovery_replay(&events, &ReplayConfig::default())
            .await
            .unwrap();

        let barrier = &report.barrier;
        assert_eq!(
            barrier.last_committed_seq, last,
            "the last successful TurnCompleted barrier stays the commit boundary"
        );
        let failure = barrier.failure.as_ref().expect("failure must be reported");
        assert_eq!(failure.seq, failed_seq);
        assert_eq!(failure.phase, "turn_completed_event");
        assert_eq!(barrier.events_after_failure, 2);
        assert!(report.seq_contiguous);
    }

    #[tokio::test]
    async fn recovery_detects_seq_gaps() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        // Drop seq 3 so the sequence is no longer contiguous.
        events.retain(|envelope| envelope.seq != 3);

        let report = recovery_replay(&events, &ReplayConfig::default())
            .await
            .unwrap();

        assert!(!report.seq_contiguous);
        assert_eq!(report.seq_gap, Some((3, 4)));
    }

    #[tokio::test]
    async fn recovery_rebuilds_context_state_deterministically() {
        let run = RunId::new();
        let events = happy_trace(run);
        let config = ReplayConfig::default();

        let first = recovery_replay(&events, &config).await.unwrap();
        let second = recovery_replay(&events, &config).await.unwrap();

        // Same trace, same engine version, same rebuilt state.
        assert_eq!(
            serde_json::to_string(&first.rebuilt_diagnostics).unwrap(),
            serde_json::to_string(&second.rebuilt_diagnostics).unwrap()
        );
        assert_eq!(
            first.rebuilt.input_tokens_total,
            second.rebuilt.input_tokens_total
        );
        assert!(first.rebuilt_diagnostics.total_items > 0);
    }

    #[tokio::test]
    async fn restore_then_incremental_replay_matches_full_rebuild() {
        let run = RunId::new();
        let events = happy_trace(run);
        let config = ReplayConfig::default();

        // Capture a checkpoint on a fresh engine after replaying the events
        // up to and including the first TurnCompleted (the barrier seq), then
        // prove that restoring it and replaying the events after it equals a
        // full rebuild of the whole trace.
        let cover_index = events
            .iter()
            .position(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted))
            .expect("happy trace has a TurnCompleted");
        let cover_seq = events[cover_index].seq;
        let engine = std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
        run_engine_observing(engine.clone(), &events[..=cover_index], &config, |_, _| {})
            .await
            .unwrap();
        let checkpoint = engine.checkpoint().await.unwrap();

        let report = verify_restore_consistency(&checkpoint, cover_seq, &events, &config)
            .await
            .unwrap();

        assert!(
            report.consistent,
            "checkpoint restore + tail replay must equal the full rebuild: {:?}",
            report.first_difference
        );
        assert_eq!(
            report.full_diagnostics.turn, report.incremental_diagnostics.turn,
            "both paths replay the same turns"
        );
    }

    #[tokio::test]
    async fn verify_restore_consistency_detects_a_wrong_cover_seq() {
        let run = RunId::new();
        let events = happy_trace(run);
        let config = ReplayConfig::default();

        // A checkpoint captured after BOTH turns, but the caller claims it
        // only covers the first barrier: the tail replay re-applies events
        // the checkpoint already contains, so the incremental state must
        // disagree with the full rebuild.
        let engine = std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
        run_engine_observing(engine.clone(), &events, &config, |_, _| {})
            .await
            .unwrap();
        let checkpoint = engine.checkpoint().await.unwrap();
        let cover_seq = events
            .iter()
            .find(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted))
            .map(|envelope| envelope.seq)
            .expect("happy trace has a TurnCompleted");

        let report = verify_restore_consistency(&checkpoint, cover_seq, &events, &config)
            .await
            .unwrap();

        assert!(
            !report.consistent,
            "replaying events the checkpoint already contains must diverge"
        );
        assert!(report.first_difference.is_some());
    }
}

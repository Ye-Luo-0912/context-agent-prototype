//! Crash-recovery replay: re-read a JSONL event trace to rebuild the
//! context-engine state and locate the durability barrier after a failed
//! turn commit.
//!
//! The runtime's explicit `RuntimeCommitBarrier` is the durability contract:
//! events at or before the last marker are durably committed,
//! and a failed barrier (`TurnCommitFailed` + `RecoveryRequired`) means the
//! turn did *not* commit — the runtime drops the turn frame and fences
//! further mutation until a known-good restore. This module answers the
//! recovery question from the trace alone:
//!
//! - where the last committed barrier stands and why (phase/message of the
//!   failure that forced recovery, if any);
//! - whether the envelope sequence is contiguous (a gap means events were
//!   lost or duplicated on disk);
//! - the context-engine state rebuilt from the committed trace prefix (fresh
//!   engine, deterministic replay) — the "truth" a recovery can trust;
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
    ContextDiagnostics, ContextEngine, RunId, RuntimeEvent, RuntimeEventEnvelope, TurnId,
};
use agent_runtime::RuntimeCheckpoint;
use context_baselines::{AppendOnlyEngine, RollingConfig, RollingSummaryEngine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::Value;

use crate::{ReplayConfig, ReplayOutcome, run_engine_observing};

/// Which context engine a recovery rebuild replays the trace against.
/// Traces record runtime events, not the engine that produced them, so
/// the caller must state the kind; silently defaulting every trace to C
/// would make the rebuilt "truth" wrong for append/rolling runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayEngineKind {
    /// `SimpleContextEngine` — the dynamic working set (policy C).
    #[default]
    Dynamic,
    /// `AppendOnlyEngine` — baseline A.
    Append,
    /// `RollingSummaryEngine` — baseline B.
    Rolling,
}

fn build_engine(kind: ReplayEngineKind) -> std::sync::Arc<dyn ContextEngine> {
    match kind {
        ReplayEngineKind::Dynamic => {
            std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()))
        }
        ReplayEngineKind::Append => std::sync::Arc::new(AppendOnlyEngine::new()),
        ReplayEngineKind::Rolling => {
            std::sync::Arc::new(RollingSummaryEngine::with_config(RollingConfig::default()))
        }
    }
}

/// Where the trace's durability barrier stands and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBarrier {
    /// Seq of the last explicit runtime commit barrier. Events at or before
    /// this seq are durably committed; a crash-recovery rebuild may treat
    /// everything before it as trusted. A new-format run may stop at its
    /// `RunStart` marker without committing a model turn. `0` means no
    /// explicit marker (and no legacy turn fallback) committed.
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
    /// The context-engine state rebuilt from the committed trace prefix: a
    /// fresh engine replaying events through `barrier.last_committed_seq` in
    /// order. The forensic suffix remains available to the diagnostics in
    /// this report but never becomes restored Context truth.
    pub rebuilt: ReplayOutcome,
    /// Final diagnostics of the rebuilt engine (the "truth" state).
    pub rebuilt_diagnostics: ContextDiagnostics,
    /// Action-batch interruption evidence: rounds killed by process loss
    /// before their settlement accounting landed, plus live settle-time
    /// integrity violations seen in the trace.
    pub batch_interruptions: BatchInterruptionReport,
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
    /// Which durable source defined truth for this proof.
    pub truth_source: RestoreTruthSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreTruthSource {
    TraceBarrier,
    TerminalCheckpoint,
}

/// Analyze the durability barrier of one run's envelopes.
pub fn analyze_barrier(events: &[RuntimeEventEnvelope]) -> RecoveryBarrier {
    let mut last_committed_seq = 0u64;
    let mut failure: Option<RecoveryFailure> = None;
    // v4 traces written before the explicit marker remain readable, but a
    // trace that contains any marker uses only marker semantics throughout.
    // New runs durably append a RunStart marker before work begins, so a
    // partially appended first turn can never fall back to legacy
    // TurnCompleted inference merely because its final marker is absent.
    let has_explicit_barrier = events
        .iter()
        .any(|envelope| matches!(envelope.event, RuntimeEvent::RuntimeCommitBarrier { .. }));
    for envelope in events {
        match &envelope.event {
            RuntimeEvent::RuntimeCommitBarrier { .. } => last_committed_seq = envelope.seq,
            RuntimeEvent::TurnCompleted if !has_explicit_barrier => {
                last_committed_seq = envelope.seq
            }
            // TurnCancelled has its own durable audit barrier, but it
            // explicitly means that no model/context turn commit occurred.
            // It must never advance the successful-commit recovery marker.
            RuntimeEvent::TurnCancelled { .. } => {}
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

/// Borrow the ordered prefix covered by the last successful runtime barrier.
///
/// Recovery traces are append-ordered by sequence. Sequence integrity is
/// reported separately over the full trace; this selector deliberately does
/// not discard the forensic suffix from those diagnostics. With no committed
/// barrier (`last_committed_seq == 0`) the trusted Context prefix is empty.
fn committed_prefix(
    events: &[RuntimeEventEnvelope],
    last_committed_seq: u64,
) -> &[RuntimeEventEnvelope] {
    let end = events
        .iter()
        .position(|envelope| envelope.seq > last_committed_seq)
        .unwrap_or(events.len());
    &events[..end]
}

/// Borrow the part of an already committed prefix not covered by a
/// checkpoint. Keeping this as a slice avoids cloning bounded event payloads
/// during offline recovery verification.
fn events_after_sequence(
    events: &[RuntimeEventEnvelope],
    covered_seq: u64,
) -> &[RuntimeEventEnvelope] {
    let start = events
        .iter()
        .position(|envelope| envelope.seq > covered_seq)
        .unwrap_or(events.len());
    &events[start..]
}

/// One model round whose tool batch never settled: abrupt process loss
/// killed the runtime after calls started but before the durable
/// `ExecutionBatchSettled` accounting landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedBatch {
    pub turn_id: TurnId,
    pub model_round: usize,
    /// seq of the `ModelStarted` that opened the round.
    pub opened_seq: u64,
    /// seq of the last event observed inside the interrupted window.
    pub last_seq: u64,
    pub started_calls: usize,
    pub finished_calls: usize,
}

/// A settled batch whose own accounting reported missing or unexpected
/// terminals. This is live integrity damage (the actor saw it while
/// settling), not crash loss — recovery must surface it separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleIntegrityViolation {
    pub seq: u64,
    pub turn_id: TurnId,
    pub model_round: usize,
    pub missing_terminal: usize,
    pub unexpected_terminal: usize,
}

/// Trace-only evidence about action batches that never received durable
/// settlement accounting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchInterruptionReport {
    pub interrupted_rounds: Vec<InterruptedBatch>,
    pub integrity_violations: Vec<SettleIntegrityViolation>,
}

struct OpenRound {
    turn_id: TurnId,
    model_round: usize,
    opened_seq: u64,
    last_seq: u64,
    started_calls: usize,
    finished_calls: usize,
}

impl OpenRound {
    fn into_interrupted(self) -> Option<InterruptedBatch> {
        // A round with no tool activity leaves nothing to account for;
        // flagging it would turn every plain text turn into a defect.
        if self.started_calls == 0 && self.finished_calls == 0 {
            return None;
        }
        Some(InterruptedBatch {
            turn_id: self.turn_id,
            model_round: self.model_round,
            opened_seq: self.opened_seq,
            last_seq: self.last_seq,
            started_calls: self.started_calls,
            finished_calls: self.finished_calls,
        })
    }
}

/// Detect interrupted tool batches from the trace alone. A healthy runtime
/// settles every batch (`ExecutionBatchSettled`) before the next model round
/// and before every terminal path; any window left open at a new round, a
/// turn boundary, or the end of the trace is therefore evidence of abrupt
/// process loss, and its per-call counts are what replay can honestly claim:
/// calls were started, some finished, and none was accounted.
///
/// Tool events outside any `ModelStarted`-anchored window are ignored — old
/// or synthetic traces may carry them, and inventing an attribution would
/// defeat the purpose of evidence.
pub fn analyze_batch_interruptions(events: &[RuntimeEventEnvelope]) -> BatchInterruptionReport {
    let mut report = BatchInterruptionReport::default();
    let mut open: Option<OpenRound> = None;
    let flush = |open: &mut Option<OpenRound>, report: &mut BatchInterruptionReport| {
        if let Some(round) = open.take()
            && let Some(interrupted) = round.into_interrupted()
        {
            report.interrupted_rounds.push(interrupted);
        }
    };

    for envelope in events {
        match &envelope.event {
            RuntimeEvent::ModelStarted {
                turn_id,
                model_round,
                ..
            } => {
                // The runtime settles before requesting the next round; an
                // open window here is itself interruption evidence.
                flush(&mut open, &mut report);
                open = Some(OpenRound {
                    turn_id: *turn_id,
                    model_round: *model_round,
                    opened_seq: envelope.seq,
                    last_seq: envelope.seq,
                    started_calls: 0,
                    finished_calls: 0,
                });
            }
            RuntimeEvent::ToolStarted { .. } => {
                if let Some(round) = open.as_mut() {
                    round.started_calls += 1;
                    round.last_seq = envelope.seq;
                }
            }
            RuntimeEvent::ToolFinished { .. } => {
                if let Some(round) = open.as_mut() {
                    round.finished_calls += 1;
                    round.last_seq = envelope.seq;
                }
            }
            RuntimeEvent::ExecutionBatchSettled {
                turn_id,
                model_round,
                missing_terminal,
                unexpected_terminal,
                ..
            } => {
                if *missing_terminal > 0 || *unexpected_terminal > 0 {
                    report.integrity_violations.push(SettleIntegrityViolation {
                        seq: envelope.seq,
                        turn_id: *turn_id,
                        model_round: *model_round,
                        missing_terminal: *missing_terminal,
                        unexpected_terminal: *unexpected_terminal,
                    });
                }
                // Settlement is the accounting authority for its window even
                // when the trace's identity fields disagree with it.
                open = None;
            }
            RuntimeEvent::TurnCompleted
            | RuntimeEvent::TurnCancelled { .. }
            | RuntimeEvent::TurnCommitFailed { .. } => flush(&mut open, &mut report),
            _ => {}
        }
    }
    // Abrupt loss usually ends the trace mid-round: whatever is still open
    // here never got its accounting.
    flush(&mut open, &mut report);
    report
}

/// Rebuild the context-engine state from one run's envelopes: a fresh
/// engine of the requested kind replays every event deterministically.
/// Returns the measurement outcome and the final diagnostics of that engine.
pub async fn rebuild_engine_state(
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    kind: ReplayEngineKind,
) -> anyhow::Result<(ReplayOutcome, ContextDiagnostics)> {
    let engine = build_engine(kind);
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
    kind: ReplayEngineKind,
) -> anyhow::Result<RecoveryReport> {
    let lines = crate::read_trace_lines(path).await?;

    let mut envelopes: Vec<RuntimeEventEnvelope> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
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

    recovery_replay(&envelopes, config, kind).await
}

/// Produce the crash-recovery report for one run's envelopes.
pub async fn recovery_replay(
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    kind: ReplayEngineKind,
) -> anyhow::Result<RecoveryReport> {
    let run_id = events
        .first()
        .map(|envelope| envelope.run_id)
        .ok_or_else(|| anyhow::anyhow!("recovery replay needs at least one event"))?;
    let barrier = analyze_barrier(events);
    let seq_gap = first_seq_gap(events);
    let batch_interruptions = analyze_batch_interruptions(events);
    let trusted = committed_prefix(events, barrier.last_committed_seq);
    let (rebuilt, rebuilt_diagnostics) = rebuild_engine_state(trusted, config, kind).await?;
    Ok(RecoveryReport {
        run_id,
        envelopes: events.len(),
        seq_contiguous: seq_gap.is_none(),
        seq_gap,
        barrier,
        rebuilt,
        rebuilt_diagnostics,
        batch_interruptions,
    })
}

/// Prove a context checkpoint agrees with committed trace truth: restore it
/// into a fresh engine, replay committed events after
/// `checkpoint_cover_seq`, and compare the resulting diagnostics with a
/// fresh rebuild of the same committed prefix.
///
/// `checkpoint_cover_seq` is the seq of the last event the checkpoint was
/// captured after — the caller knows it because a checkpoint is captured at
/// a runtime safe point and records the event cursor covered by that state.
/// The checkpoint must be captured at a complete preview/consumption
/// boundary, otherwise the incremental replay may encounter an unmatched
/// acknowledgement.
pub async fn verify_restore_consistency(
    checkpoint: &Value,
    checkpoint_cover_seq: u64,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    kind: ReplayEngineKind,
) -> anyhow::Result<RestoreConsistencyReport> {
    // Both sides prove the same committed truth. Events after the last
    // successful runtime barrier remain forensic evidence and must not be
    // resurrected into either rebuild path.
    let barrier = analyze_barrier(events);
    let trusted = committed_prefix(events, barrier.last_committed_seq);
    let (_, full_diagnostics) = rebuild_engine_state(trusted, config, kind).await?;

    // Incremental: restore the checkpoint, then replay only the events that
    // landed after it. The engine's own `restore` is a whole-state replace,
    // so the replayed tail applies on exactly the captured state.
    let engine = build_engine(kind);
    engine.restore(checkpoint.clone()).await?;
    let tail = events_after_sequence(trusted, checkpoint_cover_seq);
    run_engine_observing(engine.clone(), tail, config, |_, _| {}).await?;
    let incremental_diagnostics = engine.diagnostics().await?;

    let (consistent, first_difference) =
        compare_diagnostics(&incremental_diagnostics, &full_diagnostics);

    Ok(RestoreConsistencyReport {
        incremental_diagnostics,
        full_diagnostics,
        consistent,
        first_difference,
        truth_source: RestoreTruthSource::TraceBarrier,
    })
}

/// Verify a full runtime checkpoint without allowing an older journal
/// prefix to roll back a durably frozen terminal context plane. A matching
/// task-completion barrier becomes the checkpoint's logical cover boundary;
/// before that marker exists, the terminal checkpoint is stronger truth.
pub async fn verify_runtime_restore_consistency(
    checkpoint: &RuntimeCheckpoint,
    events: &[RuntimeEventEnvelope],
    config: &ReplayConfig,
    kind: ReplayEngineKind,
) -> anyhow::Result<RestoreConsistencyReport> {
    checkpoint.validate()?;
    if !checkpoint.terminal_commit {
        return verify_restore_consistency(
            &checkpoint.context,
            checkpoint.event_cover_seq,
            events,
            config,
            kind,
        )
        .await;
    }

    let barrier = analyze_barrier(events);
    let trusted = committed_prefix(events, barrier.last_committed_seq);
    let matching_terminal_barrier = trusted.iter().find_map(|envelope| match &envelope.event {
        RuntimeEvent::RuntimeCommitBarrier {
            kind: agent_contracts::RuntimeCommitKind::TaskCompletion,
            checkpoint_sequence: Some(sequence),
        } if *sequence == checkpoint.snapshot_sequence => Some(envelope.seq),
        _ => None,
    });
    let logical_cover_seq = matching_terminal_barrier.unwrap_or(checkpoint.event_cover_seq);

    let engine = build_engine(kind);
    engine.restore(checkpoint.context.clone()).await?;
    let tail = events_after_sequence(trusted, logical_cover_seq);
    run_engine_observing(engine.clone(), tail, config, |_, _| {}).await?;
    let incremental_diagnostics = engine.diagnostics().await?;

    let (full_diagnostics, truth_source) = if matching_terminal_barrier.is_some() {
        let (_, diagnostics) = rebuild_engine_state(trusted, config, kind).await?;
        (diagnostics, RestoreTruthSource::TraceBarrier)
    } else {
        // The event trace cannot reconstruct the terminal transaction in
        // this crash window; the atomic runtime checkpoint is authoritative.
        (
            incremental_diagnostics.clone(),
            RestoreTruthSource::TerminalCheckpoint,
        )
    };
    let (consistent, first_difference) =
        compare_diagnostics(&incremental_diagnostics, &full_diagnostics);
    Ok(RestoreConsistencyReport {
        incremental_diagnostics,
        full_diagnostics,
        consistent,
        first_difference,
        truth_source,
    })
}

/// Compare every dimension of two diagnostics snapshots, returning the first
/// mismatched dimension name when they differ.
fn compare_diagnostics(
    left: &ContextDiagnostics,
    right: &ContextDiagnostics,
) -> (bool, Option<String>) {
    let pairs: [(&str, usize, usize); 18] = [
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
        ("resident_bytes", left.resident_bytes, right.resident_bytes),
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
    let interruptions = &report.batch_interruptions;
    if interruptions.interrupted_rounds.is_empty() && interruptions.integrity_violations.is_empty()
    {
        out.push_str("action batches: every started batch settled\n");
    } else {
        for batch in &interruptions.interrupted_rounds {
            out.push_str(&format!(
                "INTERRUPTED batch: turn {} round {} (opened at seq {}, last event seq {}): {} call(s) started, {} finished, none accounted — abrupt loss evidence\n",
                batch.turn_id,
                batch.model_round,
                batch.opened_seq,
                batch.last_seq,
                batch.started_calls,
                batch.finished_calls,
            ));
        }
        for violation in &interruptions.integrity_violations {
            out.push_str(&format!(
                "SETTLE INTEGRITY: turn {} round {} settled at seq {} with {} missing / {} unexpected terminal(s)\n",
                violation.turn_id,
                violation.model_round,
                violation.seq,
                violation.missing_terminal,
                violation.unexpected_terminal,
            ));
        }
    }
    let diagnostics = &report.rebuilt_diagnostics;
    out.push_str(&format!(
        "rebuilt context: total={} active={} cooling={} archived={} resident={} bytes={} warm={} cold={} external={} | turn={} event_seq={}\n",
        diagnostics.total_items,
        diagnostics.active_items,
        diagnostics.cooling_items,
        diagnostics.archived_items,
        diagnostics.resident_items,
        diagnostics.resident_bytes,
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
        ContextSelection, OperationId, TaskId, ToolCall, ToolOutput, TurnCancellationReason,
        TurnId,
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
                materialize_ms: 0,
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
                    foreground_item_ids: Vec::new(),
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
            RuntimeEvent::user_message_accepted("fix AuthService.rs"),
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
                facts: None,
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
            RuntimeEvent::user_message_accepted("verify"),
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

    fn explicit_barriers(events: Vec<RuntimeEventEnvelope>) -> Vec<RuntimeEventEnvelope> {
        let mut explicit = Vec::with_capacity(events.len() + 2);
        for envelope in events {
            let run_id = envelope.run_id;
            let terminal = matches!(envelope.event, RuntimeEvent::TurnCompleted);
            explicit.push(envelope);
            if terminal {
                explicit.push(RuntimeEventEnvelope {
                    run_id,
                    seq: 0,
                    timestamp_ms: 0,
                    event: RuntimeEvent::RuntimeCommitBarrier {
                        kind: agent_contracts::RuntimeCommitKind::Turn,
                        checkpoint_sequence: None,
                    },
                });
            }
        }
        for (index, envelope) in explicit.iter_mut().enumerate() {
            envelope.seq = index as u64 + 1;
        }
        explicit
    }

    #[test]
    fn explicit_barrier_owns_normal_turn_and_ignores_uncommitted_terminal_tail() {
        let run = RunId::new();
        let mut events = explicit_barriers(happy_trace(run));
        let committed = events.last().unwrap().seq;
        events.push(envelope(
            run,
            committed + 1,
            RuntimeEvent::TaskCompleted {
                task_id: TaskId::new(),
                anchor_revision: 0,
                summary: "checkpoint landed but audit transaction did not".into(),
            },
        ));
        assert_eq!(analyze_barrier(&events).last_committed_seq, committed);

        events.push(envelope(
            run,
            committed + 2,
            RuntimeEvent::RuntimeCommitBarrier {
                kind: agent_contracts::RuntimeCommitKind::TaskCompletion,
                checkpoint_sequence: Some(7),
            },
        ));
        assert_eq!(analyze_barrier(&events).last_committed_seq, committed + 2);
    }

    #[test]
    fn explicit_barrier_never_falls_back_to_a_later_bare_turn_completed() {
        let run = RunId::new();
        let mut events = explicit_barriers(happy_trace(run));
        let committed = events.last().unwrap().seq;
        events.push(envelope(run, committed + 1, RuntimeEvent::TurnCompleted));
        events.push(envelope(
            run,
            committed + 2,
            RuntimeEvent::TurnCommitFailed {
                phase: "runtime_commit_barrier".into(),
                message: "flush failed".into(),
            },
        ));
        let barrier = analyze_barrier(&events);
        assert_eq!(barrier.last_committed_seq, committed);
        assert_eq!(barrier.failure.unwrap().seq, committed + 2);
    }

    #[test]
    fn run_start_marker_prevents_legacy_fallback_for_a_partial_first_turn_batch() {
        let run = RunId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            envelope(
                run,
                2,
                RuntimeEvent::RuntimeCommitBarrier {
                    kind: agent_contracts::RuntimeCommitKind::RunStart,
                    checkpoint_sequence: None,
                },
            ),
            // The lifecycle member reached the journal, but appending the
            // final Turn marker failed. This must not look like a committed
            // legacy turn.
            envelope(run, 3, RuntimeEvent::TurnCompleted),
            envelope(
                run,
                4,
                RuntimeEvent::TurnCommitFailed {
                    phase: "runtime_commit_barrier".into(),
                    message: "marker append failed".into(),
                },
            ),
        ];

        let barrier = analyze_barrier(&events);
        assert_eq!(barrier.last_committed_seq, 2);
        assert_eq!(barrier.failure.unwrap().seq, 4);
    }

    #[tokio::test]
    async fn recovery_locates_the_last_committed_barrier() {
        let run = RunId::new();
        let events = happy_trace(run);
        let report = recovery_replay(&events, &ReplayConfig::default(), ReplayEngineKind::Dynamic)
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
            RuntimeEvent::user_message_accepted("third turn"),
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

        let report = recovery_replay(&events, &ReplayConfig::default(), ReplayEngineKind::Dynamic)
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
    async fn recovery_rebuild_excludes_the_uncommitted_tail_but_keeps_its_diagnostics() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        let committed_seq = events.last().unwrap().seq;
        let failed_turn = TurnId::new();
        let mut seq = committed_seq + 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::user_message_accepted("uncommitted third turn"),
        ));
        seq += 1;
        events.push(model_started_event(run, seq, failed_turn, 3));
        seq += 1;
        events.push(tool_started_event(run, seq, "tail-call"));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: tool_output(true, "uncommitted tail observation"),
                facts: None,
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::TurnCommitFailed {
                phase: "turn_completed_event".into(),
                message: "journal flush failed".into(),
            },
        ));
        seq += 1;
        events.push(envelope(run, seq, RuntimeEvent::RecoveryRequired));

        let report = recovery_replay(&events, &ReplayConfig::default(), ReplayEngineKind::Dynamic)
            .await
            .unwrap();

        assert_eq!(
            report.envelopes,
            events.len(),
            "the forensic trace stays intact"
        );
        assert_eq!(report.barrier.last_committed_seq, committed_seq);
        assert_eq!(
            report.rebuilt.events_consumed, committed_seq as usize,
            "Context truth must stop at the last successful turn barrier"
        );
        assert_eq!(
            report.rebuilt.turns, 2,
            "the failed third turn is not restored"
        );
        assert_eq!(
            report.rebuilt.tool_rounds, 1,
            "the uncommitted tool observation is not restored"
        );
        assert_eq!(
            report.batch_interruptions.interrupted_rounds.len(),
            1,
            "full-trace interruption diagnostics still inspect the failed suffix"
        );
    }

    #[tokio::test]
    async fn recovery_with_no_successful_turn_rebuilds_empty_context_truth() {
        let run = RunId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            envelope(
                run,
                2,
                RuntimeEvent::user_message_accepted("never committed"),
            ),
            envelope(
                run,
                3,
                RuntimeEvent::AssistantMessage {
                    content: "must remain forensic".into(),
                },
            ),
            envelope(
                run,
                4,
                RuntimeEvent::TurnCommitFailed {
                    phase: "assistant_observation".into(),
                    message: "context ingest failed".into(),
                },
            ),
            envelope(run, 5, RuntimeEvent::RecoveryRequired),
        ];

        let report = recovery_replay(&events, &ReplayConfig::default(), ReplayEngineKind::Dynamic)
            .await
            .unwrap();

        assert_eq!(report.barrier.last_committed_seq, 0);
        assert_eq!(report.rebuilt.events_consumed, 0);
        assert_eq!(report.rebuilt.turns, 0);
        assert_eq!(report.rebuilt_diagnostics.total_items, 0);
        assert_eq!(report.envelopes, events.len());
        assert!(report.barrier.failure.is_some());
    }

    #[test]
    fn cancelled_turn_does_not_advance_the_successful_commit_barrier() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        let last_committed = events.last().unwrap().seq;
        let cancelled_turn = TurnId::new();
        events.push(envelope(
            run,
            last_committed + 1,
            RuntimeEvent::user_message_accepted("cancel this"),
        ));
        events.push(envelope(
            run,
            last_committed + 2,
            RuntimeEvent::TurnCancelled {
                turn_id: cancelled_turn,
                task_id: None,
                operation_id: Some(OperationId::new()),
                cancelled_generation: 7,
                effective_generation: 8,
                reason: TurnCancellationReason::Requested,
            },
        ));

        let barrier = analyze_barrier(&events);
        assert_eq!(
            barrier.last_committed_seq, last_committed,
            "a durable cancellation is an audit fact, not a successful model/context commit"
        );
        assert_eq!(barrier.failure, None);
    }

    #[tokio::test]
    async fn recovery_detects_seq_gaps() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        // Drop seq 3 so the sequence is no longer contiguous.
        events.retain(|envelope| envelope.seq != 3);

        let report = recovery_replay(&events, &ReplayConfig::default(), ReplayEngineKind::Dynamic)
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

        let first = recovery_replay(&events, &config, ReplayEngineKind::Dynamic)
            .await
            .unwrap();
        let second = recovery_replay(&events, &config, ReplayEngineKind::Dynamic)
            .await
            .unwrap();

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
    async fn recovery_rebuild_honors_the_requested_engine_kind() {
        let run = RunId::new();
        let events = happy_trace(run);
        let config = ReplayConfig::default();

        let report = recovery_replay(&events, &config, ReplayEngineKind::Append)
            .await
            .unwrap();

        assert_eq!(report.rebuilt.turns, 2);
        assert!(report.rebuilt_diagnostics.total_items > 0);
        assert_eq!(
            report.rebuilt_diagnostics.resident_items, report.rebuilt_diagnostics.total_items,
            "the append baseline keeps every record resident"
        );

        // The rolling kind must also construct and replay cleanly.
        let rolling = recovery_replay(&events, &config, ReplayEngineKind::Rolling)
            .await
            .unwrap();
        assert_eq!(rolling.rebuilt.turns, 2);
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

        let report = verify_restore_consistency(
            &checkpoint,
            cover_seq,
            &events,
            &config,
            ReplayEngineKind::Dynamic,
        )
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
    async fn durable_terminal_checkpoint_wins_the_pre_audit_crash_window() {
        use agent_runtime::task::{
            CompletionDisposition, CompletionRecord, CompletionVerificationStatus, TaskAnchor,
            TaskStatus, TaskToolRequirementSet,
        };
        use agent_runtime::{
            ExecutionState, RunMetadata, RuntimeCheckpoint, TaskManagerSnapshot, TaskRecordSnapshot,
        };

        let run = RunId::new();
        let task_id = TaskId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::FocusChanged {
                    task_id,
                    goal: "finish atomically".into(),
                },
            ),
            envelope(run, 2, RuntimeEvent::TurnCompleted),
            envelope(
                run,
                3,
                RuntimeEvent::RuntimeCommitBarrier {
                    kind: agent_contracts::RuntimeCommitKind::Turn,
                    checkpoint_sequence: None,
                },
            ),
        ];

        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(agent_contracts::ContextIngress::FocusChanged {
                focus: agent_contracts::FocusState::for_task(task_id, "finish atomically"),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::FocusChanged)
            .await
            .unwrap();
        engine
            .ingest(agent_contracts::ContextIngress::TaskCompleted {
                task_id: Some(task_id),
                summary: "done".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::TaskCompleted)
            .await
            .unwrap();

        let completion = CompletionRecord {
            task_id,
            anchor_revision: 0,
            summary: "done".into(),
            completed_at_ms: 1,
            final_output_ref: None,
            final_output_digest: None,
            artifacts: Vec::new(),
            verification_status: CompletionVerificationStatus::Unverified,
            verification_refs: Vec::new(),
            disposition: CompletionDisposition::LegacyUnclassified,
            unmet_reasons: Vec::new(),
        };
        let checkpoint = RuntimeCheckpoint {
            version: agent_runtime::RUNTIME_CHECKPOINT_VERSION,
            run_metadata: RunMetadata {
                run_id: run,
                created_at_ms: 1,
            },
            tasks: TaskManagerSnapshot {
                tasks: vec![TaskRecordSnapshot {
                    id: task_id,
                    goal: "finish atomically".into(),
                    status: TaskStatus::Completed,
                    created_at_ms: 0,
                    last_active_ms: 1,
                    tool_requirements: TaskToolRequirementSet::default(),
                    anchor: TaskAnchor::default(),
                    resume: ExecutionState::default(),
                    turn_intent: String::new(),
                }],
                active: None,
                completed: vec![completion],
            },
            current_task_id: None,
            focus_revision: 1,
            last_surface_revision: 0,
            context: engine.checkpoint().await.unwrap(),
            capabilities: Vec::new(),
            authority: None,
            snapshot_sequence: 7,
            capability_generation: 0,
            event_cover_seq: 3,
            terminal_commit: true,
        };

        let report = verify_runtime_restore_consistency(
            &checkpoint,
            &events,
            &ReplayConfig::default(),
            ReplayEngineKind::Dynamic,
        )
        .await
        .unwrap();
        assert!(report.consistent);
        assert_eq!(report.truth_source, RestoreTruthSource::TerminalCheckpoint);
        assert!(
            report.incremental_diagnostics.total_items > 0,
            "recovery must retain the terminal context rather than rebuilding an older prefix"
        );
    }

    #[tokio::test]
    async fn restore_consistency_ignores_events_after_the_last_committed_barrier() {
        let run = RunId::new();
        let mut events = happy_trace(run);
        let cover_index = events
            .iter()
            .position(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted))
            .expect("happy trace has a first committed turn");
        let cover_seq = events[cover_index].seq;
        let engine = std::sync::Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
        run_engine_observing(
            engine.clone(),
            &events[..=cover_index],
            &ReplayConfig::default(),
            |_, _| {},
        )
        .await
        .unwrap();
        let checkpoint = engine.checkpoint().await.unwrap();

        let mut seq = events.last().unwrap().seq + 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::user_message_accepted("failed third turn"),
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::AssistantMessage {
                content: "uncommitted answer".into(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::TurnCommitFailed {
                phase: "turn_completed_event".into(),
                message: "barrier failed".into(),
            },
        ));

        let report = verify_restore_consistency(
            &checkpoint,
            cover_seq,
            &events,
            &ReplayConfig::default(),
            ReplayEngineKind::Dynamic,
        )
        .await
        .unwrap();

        assert!(
            report.consistent,
            "checkpoint plus committed tail must equal committed full rebuild: {:?}",
            report.first_difference
        );
        assert_eq!(report.full_diagnostics.turn, 2);
        assert_eq!(report.incremental_diagnostics.turn, 2);
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

        let report = verify_restore_consistency(
            &checkpoint,
            cover_seq,
            &events,
            &config,
            ReplayEngineKind::Dynamic,
        )
        .await
        .unwrap();

        assert!(
            !report.consistent,
            "replaying events the checkpoint already contains must diverge"
        );
        assert!(report.first_difference.is_some());
    }

    fn model_started_event(
        run: RunId,
        seq: u64,
        turn: TurnId,
        round: usize,
    ) -> RuntimeEventEnvelope {
        envelope(
            run,
            seq,
            RuntimeEvent::ModelStarted {
                turn_id: turn,
                operation_id: OperationId::new(),
                generation: 1,
                surface_revision: 0,
                model_round: round,
                prompt_layers: agent_contracts::PromptLayerCosts::default(),
                turn_checkpoint: agent_contracts::TurnCheckpointStats::default(),
            },
        )
    }

    fn tool_started_event(run: RunId, seq: u64, call_id: &str) -> RuntimeEventEnvelope {
        envelope(
            run,
            seq,
            RuntimeEvent::ToolStarted {
                call: ToolCall {
                    id: call_id.into(),
                    name: "fs.read".into(),
                    arguments: json!({}),
                },
            },
        )
    }

    fn settled_event(
        run: RunId,
        seq: u64,
        turn: TurnId,
        round: usize,
        missing_terminal: usize,
        unexpected_terminal: usize,
    ) -> RuntimeEventEnvelope {
        envelope(
            run,
            seq,
            RuntimeEvent::ExecutionBatchSettled {
                turn_id: turn,
                model_round: round,
                requested: 1,
                terminal: 1 - missing_terminal.min(1),
                spawned: 1,
                refused: 0,
                reused: 0,
                persist_observation: 1,
                transient_no_persist: 0,
                access_event_only: 0,
                succeeded: 1,
                failed: 0,
                known_mutation_results: 1,
                typed_verification_results: 0,
                unknown_invalidations: 0,
                completion_proposals: 0,
                outcome_advances: 1,
                no_outcome_results: 0,
                missing_terminal,
                unexpected_terminal,
            },
        )
    }

    #[test]
    fn abrupt_loss_leaves_an_interrupted_batch_in_the_trace() {
        let run = RunId::new();
        let turn = TurnId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            model_started_event(run, 2, turn, 1),
            tool_started_event(run, 3, "call-1"),
            tool_started_event(run, 4, "call-2"),
            envelope(
                run,
                5,
                RuntimeEvent::ToolFinished {
                    output: tool_output(true, "one result landed before the loss"),
                    facts: None,
                },
            ),
        ];

        let report = analyze_batch_interruptions(&events);

        assert_eq!(
            report.interrupted_rounds,
            vec![InterruptedBatch {
                turn_id: turn,
                model_round: 1,
                opened_seq: 2,
                last_seq: 5,
                started_calls: 2,
                finished_calls: 1,
            }],
            "the trace ends mid-round: two calls started, one finished, none accounted"
        );
        assert!(report.integrity_violations.is_empty());
    }

    #[test]
    fn settled_batches_and_plain_text_rounds_are_never_flagged() {
        let run = RunId::new();
        let turn = TurnId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            model_started_event(run, 2, turn, 1),
            tool_started_event(run, 3, "call-1"),
            envelope(
                run,
                4,
                RuntimeEvent::ToolFinished {
                    output: tool_output(true, "settled normally"),
                    facts: None,
                },
            ),
            settled_event(run, 5, turn, 1, 0, 0),
            envelope(run, 6, RuntimeEvent::TurnCompleted),
            // A plain text round without any tool activity is normal.
            model_started_event(run, 7, turn, 2),
            envelope(run, 8, RuntimeEvent::TurnCompleted),
        ];

        assert_eq!(
            analyze_batch_interruptions(&events),
            BatchInterruptionReport::default()
        );
    }

    #[test]
    fn a_new_model_round_over_an_unsettled_batch_is_interruption_evidence() {
        let run = RunId::new();
        let turn = TurnId::new();
        let events = vec![
            model_started_event(run, 1, turn, 1),
            tool_started_event(run, 2, "call-1"),
            // The runtime always settles before requesting the next round;
            // this window was killed without accounting.
            model_started_event(run, 3, turn, 2),
            tool_started_event(run, 4, "call-2"),
            settled_event(run, 5, turn, 2, 0, 0),
        ];

        let report = analyze_batch_interruptions(&events);

        assert_eq!(
            report.interrupted_rounds,
            vec![InterruptedBatch {
                turn_id: turn,
                model_round: 1,
                opened_seq: 1,
                last_seq: 2,
                started_calls: 1,
                finished_calls: 0,
            }],
        );
    }

    #[test]
    fn settle_accounting_mismatch_is_a_live_integrity_violation_not_crash_evidence() {
        let run = RunId::new();
        let turn = TurnId::new();
        let events = vec![
            model_started_event(run, 1, turn, 1),
            tool_started_event(run, 2, "call-1"),
            envelope(
                run,
                3,
                RuntimeEvent::ToolFinished {
                    output: tool_output(true, "finished but unaccounted"),
                    facts: None,
                },
            ),
            settled_event(run, 4, turn, 1, 1, 0),
            envelope(run, 5, RuntimeEvent::TurnCompleted),
        ];

        let report = analyze_batch_interruptions(&events);

        assert!(report.interrupted_rounds.is_empty());
        assert_eq!(
            report.integrity_violations,
            vec![SettleIntegrityViolation {
                seq: 4,
                turn_id: turn,
                model_round: 1,
                missing_terminal: 1,
                unexpected_terminal: 0,
            }],
        );
    }

    #[tokio::test]
    async fn recovery_replay_reports_the_interrupted_batch_end_to_end() {
        let run = RunId::new();
        let turn = TurnId::new();
        let events = vec![
            envelope(run, 1, RuntimeEvent::RunStarted),
            model_started_event(run, 2, turn, 1),
            tool_started_event(run, 3, "call-1"),
            envelope(
                run,
                4,
                RuntimeEvent::ToolFinished {
                    output: tool_output(true, "lost mid-batch"),
                    facts: None,
                },
            ),
        ];
        let config = ReplayConfig::default();

        let report = recovery_replay(&events, &config, ReplayEngineKind::Dynamic)
            .await
            .unwrap();

        assert_eq!(report.batch_interruptions.interrupted_rounds.len(), 1);
        let rendered = render_recovery_report(&report);
        assert!(
            rendered.contains("INTERRUPTED batch"),
            "rendered recovery report must surface the interruption evidence:\n{rendered}"
        );
    }
}

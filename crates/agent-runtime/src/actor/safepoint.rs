//! Safe-point resume commits and background checkpoint writes.
//!
//! At a fully settled tool batch (terminal settlement for every member,
//! no operation in flight) accrued checkpoint debt freezes one candidate
//! snapshot and schedules exactly one atomic checkpoint write. Read-only
//! exploration accrues nothing.
//!
//! Every frozen snapshot carries an actor-owned monotonic `sequence`,
//! allocated independently of task-anchor revisions: two snapshots under
//! the same anchor revision never alias, and a task switch cannot move
//! ordering backwards. The write is acknowledged against that exact
//! sequence; the durable watermark advances only for it. Freezing MOVES
//! the accrued debt out of the live set into the in-flight artifact, so
//! debt accrued after the capture — including a same-reason mutation —
//! stays live for the next safe point: an acknowledgement retires only
//! the exact generation it froze (ACK(S_v) clears only debt frozen into
//! S_v). A failed write hands its frozen set back to the live debt, so
//! nothing is ever silently cleared.

use super::*;
use crate::checkpoint::{CheckpointDebtReason, CheckpointStore, StoredCheckpoint};

/// One background write in flight: its join handle, the snapshot sequence
/// it acknowledges, and the exact debt set it froze (moved out of the
/// live debt at freeze time). That frozen set is the ONLY thing this
/// acknowledgement may retire; a failed or errored write hands it back
/// to the live debt set.
/// EXEC-7 (R2-08): a safe-point checkpoint whose Checkpoint-trigger
/// maintenance runs as a spawned prepare task. The frozen debt and the
/// allocated sequence live here until a barrier or the settled-batch pump
/// lands the prepare (maintenance report applied, planes captured,
/// validated, serialized) and hands the bytes to the in-flight write —
/// preserving the synchronous durability protocol's observable state while
/// the actor's command branch stays free during a slow engine.
pub(super) struct PendingCheckpointPrepare {
    pub(super) handle: tokio::task::JoinHandle<AgentResult<ContextMaintenanceReport>>,
    pub(super) sequence: u64,
    pub(super) anchor_revision: u64,
    pub(super) captured_debt: Vec<CheckpointDebtReason>,
}

pub(super) struct InFlightCheckpoint {
    handle: tokio::task::JoinHandle<AgentResult<(u64, StoredCheckpoint)>>,
    /// The active task's anchor revision at freeze time, surfaced on the
    /// acknowledgement for observability only.
    anchor_revision: u64,
    /// Capability generation the captured plane verified stable against.
    capability_generation: u64,
    captured_debt: Vec<CheckpointDebtReason>,
}

impl InFlightCheckpoint {
    fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }
}

impl RuntimeActor {
    /// The active task's anchor revision, or zero when no task is active.
    /// Observability only: durability ordering follows the snapshot
    /// sequence, never this value.
    pub(super) fn current_anchor_revision(&self) -> u64 {
        self.state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.anchor.revision)
            .unwrap_or_default()
    }

    /// Record one coalesced reason the next settled batch owes a durable
    /// checkpoint. Idempotent per reason among not-yet-frozen debt: a
    /// reason currently frozen into an in-flight write can accrue again
    /// and stays outstanding until a LATER snapshot captures it — the
    /// in-flight acknowledgement retires only what it froze.
    pub(super) fn accrue_checkpoint_debt(&mut self, reason: CheckpointDebtReason) {
        if !self.state.checkpoint_debt.contains(&reason) {
            self.state.checkpoint_debt.push(reason);
        }
    }

    /// Hand a failed write's frozen debt back to the live set, merging
    /// without duplicates so a re-accrued reason stays a single entry.
    /// Failure must keep every reason visible and retryable.
    pub(super) fn restore_checkpoint_debt(&mut self, reasons: &[CheckpointDebtReason]) {
        for reason in reasons {
            if !self.state.checkpoint_debt.contains(reason) {
                self.state.checkpoint_debt.push(*reason);
            }
        }
    }

    /// Assemble the runtime checkpoint from every plane this process can
    /// see, under the ONE capture contract the external instance path also
    /// uses: the capability-surface generation is read before the actor,
    /// context and authority planes and re-checked after; a mismatch means
    /// a surface mutation raced the capture, so the whole assembly retries
    /// against one stable generation instead of shipping a torn view.
    /// A terminal override freezes the prospective post-completion task
    /// plane together with the already-prepared post-completion context
    /// plane and its next focus revision.
    pub(super) async fn assemble_checkpoint(
        &self,
        terminal_override: Option<(crate::checkpoint::TaskManagerSnapshot, u64)>,
    ) -> AgentResult<RuntimeCheckpoint> {
        // EXEC-7 (R2-08): the Checkpoint-trigger maintenance no longer runs
        // inside the assembly — it is the spawned prepare task, landed (and
        // its report applied) before any caller assembles. A fenced Core
        // receives a pure snapshot with no maintenance claim (the fence is
        // evaluated when the prepare is scheduled).
        let registry = self.services.capability_registry();
        let mut last_error: Option<AgentError> = None;
        for _ in 0..3 {
            let generation_before = registry.map(|registry| registry.generation());
            let context = match self.core.checkpoint().await {
                Ok(context) => context,
                Err(error) => return Err(error),
            };
            let authority = self.core.authority_checkpoint_marker()?;
            let capabilities = match registry {
                Some(registry) => registry.snapshot(),
                None => Vec::new(),
            };
            if generation_before == registry.map(|registry| registry.generation()) {
                let capability_generation = registry
                    .map(|registry| registry.generation())
                    .unwrap_or_default();
                let (tasks_snapshot, current_task_id, focus_revision) = match &terminal_override {
                    Some((tasks, focus_revision)) => (tasks.clone(), None, *focus_revision),
                    None => (
                        crate::checkpoint::TaskManagerSnapshot::from_manager(&self.state.tasks),
                        self.state.task_id,
                        self.state.focus_revision,
                    ),
                };
                return Ok(RuntimeCheckpoint {
                    version: crate::checkpoint::RUNTIME_CHECKPOINT_VERSION,
                    run_metadata: crate::checkpoint::RunMetadata {
                        run_id: self.core.run_id(),
                        created_at_ms: now_ms(),
                        provider_profile_digest: self
                            .services
                            .provider_profile_digest()
                            .unwrap_or_default()
                            .to_string(),
                    },
                    tasks: tasks_snapshot,
                    current_task_id,
                    focus_revision,
                    last_surface_revision: self.state.last_surface_revision,
                    context,
                    capabilities,
                    authority,
                    snapshot_sequence: self.state.snapshot_sequence,
                    capability_generation,
                    unresolved_ack_debts: self.state.unresolved_ack_debts.clone(),
                    event_cover_seq: self.core.event_sequence(),
                    terminal_commit: terminal_override.is_some(),
                });
            }
            // A capability surface mutation landed between the two planes;
            // retry the whole capture against one stable generation.
            last_error = Some(AgentError::Internal(
                "capability surface kept changing during checkpoint capture".into(),
            ));
        }
        Err(last_error.unwrap_or_else(|| {
            AgentError::Internal(
                "capability surface kept changing during checkpoint capture".into(),
            )
        }))
    }

    /// EXEC-7: the maintenance-free capture — the Checkpoint-trigger
    /// maintenance runs as the spawned prepare (or is skipped under a
    /// Core fence) before any caller assembles.
    pub(super) async fn capture_checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        self.assemble_checkpoint(None).await
    }

    pub(super) fn checkpoint_store(&self) -> Option<CheckpointStore> {
        self.services
            .artifact_workspace()
            .map(|workspace| CheckpointStore::new(workspace.state_dir().join("checkpoints")))
    }

    pub(super) fn checkpoint_store_missing_error() -> AgentError {
        AgentError::InvalidRequest("no checkpoint store configured".into())
    }

    /// Drain one finished background write. A success advances the durable
    /// sequence watermark to the acked snapshot and publishes
    /// `CheckpointDurable` carrying the identity tuple; the frozen debt
    /// set was already separated from the live set at freeze time, so
    /// anything still accrued — including same-reason debt from mutations
    /// that happened mid-flight — survives for the next safe point. A
    /// failure restores the frozen set to the live debt and surfaces an
    /// error so barrier callers fail closed.
    async fn take_settled_checkpoint_write(&mut self) -> AgentResult<()> {
        // EXEC-7: a finished prepare lands first so the write starts even
        // without an explicit barrier caller.
        if self
            .state
            .checkpoint_prepare
            .as_ref()
            .is_some_and(|prepare| prepare.handle.is_finished())
            && let Some(prepare) = self.state.checkpoint_prepare.take()
        {
            let report = match prepare.handle.await {
                Ok(report) => report,
                Err(join_error) => {
                    self.restore_checkpoint_debt(&prepare.captured_debt);
                    let error = AgentError::InvalidRequest(format!(
                        "checkpoint prepare task failed: {join_error}"
                    ));
                    self.emit_checkpoint_write_failed(error.to_string()).await;
                    return Err(error);
                }
            };
            self.land_safepoint_write(
                prepare.sequence,
                prepare.anchor_revision,
                prepare.captured_debt,
                Some(report),
            )
            .await;
        }
        let finished = matches!(
            self.state.checkpoint_write.as_ref(),
            Some(in_flight) if in_flight.is_finished()
        );
        if !finished {
            return Ok(());
        }
        let in_flight = self
            .state
            .checkpoint_write
            .take()
            .expect("a finished handle is present");
        match in_flight.handle.await {
            Ok(Ok((sequence, stored))) => {
                self.state.checkpoint_write_failed = false;
                self.state.durable_sequence =
                    Some(self.state.durable_sequence.unwrap_or(0).max(sequence));
                // Typed retirement: the frozen set left the live debt at
                // freeze time, so there is nothing to subtract here — debt
                // accrued after that freeze was never this artifact's to
                // clear.
                let anchor_revision = in_flight.anchor_revision;
                let capability_generation = in_flight.capability_generation;
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision: anchor_revision,
                        checksum: stored.checksum,
                        sequence,
                        capability_generation,
                    })
                    .await;
                Ok(())
            }
            Ok(Err(error)) => {
                self.restore_checkpoint_debt(&in_flight.captured_debt);
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
            Err(join_error) => {
                self.restore_checkpoint_debt(&in_flight.captured_debt);
                let error = AgentError::InvalidRequest(format!(
                    "checkpoint write task failed: {join_error}"
                ));
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
        }
    }

    pub(super) async fn emit_checkpoint_write_failed(&mut self, detail: String) {
        self.state.checkpoint_write_failed = true;
        let _ = self
            .core
            .emit_event(RuntimeEvent::CheckpointWriteFailed {
                reason: bounded_preview(&detail, agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS),
            })
            .await;
    }

    /// LONG-TASK SAFE POINT: the whole requested batch has terminal
    /// settlement and nothing is in flight. Debt coalesces into one
    /// candidate snapshot; several mutations in one batch produce one
    /// resume install and one write. The newly allocated sequence becomes
    /// continuation's required watermark. Freezing moves the accrued debt
    /// into the in-flight artifact, so debt accrued while that write is in
    /// flight — including a re-accrued reason — stays in the live set for
    /// the very next settled batch to capture as a further snapshot
    /// instead of letting the first acknowledgement silently absorb it.
    pub(super) async fn safe_point_resume_commit(&mut self) {
        let _ = self.take_settled_checkpoint_write().await;
        if self.state.checkpoint_debt.is_empty()
            || self.state.checkpoint_write.is_some()
            || self.state.checkpoint_prepare.is_some()
        {
            return;
        }
        let Some(task_id) = self.state.tasks.active() else {
            return;
        };
        let anchor_revision = self
            .state
            .tasks
            .get(task_id)
            .map(|task| task.anchor.revision)
            .unwrap_or_default();

        // Allocate this snapshot's identity before freezing any plane so
        // the written payload embeds its own sequence.
        self.state.snapshot_sequence = self
            .state
            .snapshot_sequence
            .checked_add(1)
            .expect("snapshot sequence cannot overflow within any realistic run");
        let sequence = self.state.snapshot_sequence;
        if let Some(turn) = self.state.turn.as_ref() {
            self.state
                .tasks
                .install_resume(task_id, turn.execution.clone());
        }
        self.state.required_sequence =
            Some(self.state.required_sequence.unwrap_or(0).max(sequence));

        let debt: Vec<String> = self
            .state
            .checkpoint_debt
            .iter()
            .map(|reason| reason.name().to_string())
            .collect();
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::TaskResumeCommitted {
                task_id,
                anchor_revision,
                debt,
                sequence,
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
        }

        // Freeze the batch's debt OUT of the live set: the in-flight
        // acknowledgement owns exactly these reasons, while a same-reason
        // mutation after this point accrues fresh debt that this ack can
        // never clear (ACK(S_v) retires only debt frozen into S_v).
        let captured_debt = std::mem::take(&mut self.state.checkpoint_debt);
        self.schedule_checkpoint_write(sequence, anchor_revision, captured_debt)
            .await;
    }

    /// Capture the current planes under the already-allocated sequence and
    /// hand one atomic write to the background. The frozen debt moves with
    /// the artifact: only a successful acknowledgement retires it
    /// permanently; every failure path here hands the set back to the
    /// live debt, keeping everything visible and retryable, including an
    /// impossible-by-configuration store.
    ///
    /// EXEC-7 (R2-08): the Checkpoint-trigger maintenance runs as a spawned
    /// PREPARE task, not on the actor's command branch. The synchronous
    /// durability protocol is preserved by ownership, not by blocking: the
    /// frozen debt and the sequence live in `checkpoint_prepare`, and every
    /// barrier (`await_pending_checkpoint`) — plus the settled-batch pump —
    /// lands the prepare before anything may claim the safe point settled.
    async fn schedule_checkpoint_write(
        &mut self,
        sequence: u64,
        anchor_revision: u64,
        captured_debt: Vec<CheckpointDebtReason>,
    ) {
        let fenced = matches!(
            self.core.recovery_status(),
            agent_contracts::AuthorityRecoveryStatus::RecoveryRequired { .. }
        );
        if fenced {
            // A fenced Core receives a pure snapshot with no maintenance
            // claim — inline, byte-for-byte the no-maintenance behavior.
            self.land_safepoint_write(sequence, anchor_revision, captured_debt, None)
                .await;
            return;
        }
        let context = self.services.context_engine();
        let handle = tokio::spawn(async move {
            context
                .maintain(ContextMaintenanceTrigger::Checkpoint)
                .await
        });
        self.state.checkpoint_prepare = Some(PendingCheckpointPrepare {
            handle,
            sequence,
            anchor_revision,
            captured_debt,
        });
    }

    /// EXEC-7: land a safe-point prepare — apply the maintenance report (or
    /// the fenced no-report), capture the planes maintenance-free, validate,
    /// serialize and hand the bytes to the in-flight write. Every failure
    /// path hands the frozen debt back to the live set; a failed assembly
    /// never propagates past the safe point (matching the inline behavior it
    /// replaces: CheckpointWriteFailed plus the continuation fence).
    pub(super) async fn land_safepoint_write(
        &mut self,
        sequence: u64,
        anchor_revision: u64,
        captured_debt: Vec<CheckpointDebtReason>,
        report: Option<AgentResult<ContextMaintenanceReport>>,
    ) {
        if let Some(Ok(maintain_report)) = report
            && let Err(error) = self
                .emit_context_maintained(ContextMaintenanceTrigger::Checkpoint, maintain_report)
                .await
        {
            self.restore_checkpoint_debt(&captured_debt);
            self.emit_checkpoint_write_failed(error.to_string()).await;
            return;
        }
        let capture = self.capture_checkpoint().await;
        let snapshot = match capture {
            Ok(snapshot) => {
                // The checkpoint is untrusted input the moment it exists:
                // validate before persisting so an internally inconsistent
                // plane can never become a durable acknowledgement.
                if let Err(error) = snapshot.validate() {
                    self.restore_checkpoint_debt(&captured_debt);
                    self.emit_checkpoint_write_failed(format!(
                        "assembled checkpoint failed validation: {error}"
                    ))
                    .await;
                    return;
                }
                snapshot
            }
            Err(error) => {
                self.restore_checkpoint_debt(&captured_debt);
                self.emit_checkpoint_write_failed(error.to_string()).await;
                return;
            }
        };
        let capability_generation = snapshot.capability_generation;
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.restore_checkpoint_debt(&captured_debt);
                self.emit_checkpoint_write_failed(format!(
                    "checkpoint serialization failed: {error}"
                ))
                .await;
                return;
            }
        };
        let Some(store) = self.checkpoint_store() else {
            self.restore_checkpoint_debt(&captured_debt);
            self.state.checkpoint_write_failed = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::CheckpointWriteFailed {
                    reason: Self::checkpoint_store_missing_error().to_string(),
                })
                .await;
            return;
        };
        self.state.checkpoint_write = Some(InFlightCheckpoint {
            handle: tokio::spawn(async move {
                store
                    .write_atomic(&bytes)
                    .await
                    .map(|stored| (sequence, stored))
            }),
            anchor_revision,
            capability_generation,
            captured_debt,
        });
    }

    /// EXEC-7: the turn-commit path's non-blocking turn-end barrier. A
    /// parked safe-point prepare is NOT awaited here (that would block the
    /// actor's command branch on the engine): a relay task carries the
    /// maintenance report home through the operation lane, the turn's
    /// remaining commit tail parks behind it as
    /// `GcContinuation::SafepointCommit`, and the resume lands the prepare
    /// (with its own frozen debt bookkeeping) before running the tail —
    /// preserving the resume-before-TurnCompleted durable ordering without
    /// giving up the command branch. Returns true when the tail was parked.
    pub(super) async fn relay_parked_checkpoint_prepare(
        &mut self,
        content: String,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) -> bool {
        let Some(mut prepare) = self.state.checkpoint_prepare.take() else {
            return false;
        };
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let run_id = self.core.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        let turn_id = self.state.turn.as_ref().map(|turn| turn.turn_id);
        let cancel = CancellationToken::new();
        if let Some(turn) = self.state.turn.as_mut() {
            turn.op = Some(InFlightOp {
                operation_id,
                turn_id: turn.turn_id,
                generation,
                kind: OpKind::Gc,
                scope_id: None,
                tool_identity: None,
                cancel: cancel.clone(),
                // The relay is cheap and must not be killed: aborting it
                // would orphan the parked report. Cancellation drops the
                // parked tail instead (see `cancel_pending_gc_work`).
                abort: None,
            });
        }
        let maintain_handle = prepare.handle;
        // The report fans out to two consumers: the completion relay (which
        // resumes the parked turn-commit tail) and the parked prepare's own
        // handle (which blocking barriers land through).
        let (report_tx, report_rx) = tokio::sync::oneshot::channel();
        let relay_op_tx = op_tx.clone();
        let relay = tokio::spawn(async move {
            let report = maintain_handle.await.unwrap_or_else(|join_error| {
                Err(AgentError::InvalidRequest(format!(
                    "checkpoint prepare task failed: {join_error}"
                )))
            });
            // AgentError is not Clone: map a transport failure to the typed
            // storage error for the barrier copy, keep the original for the
            // tail resume.
            let for_barrier = match &report {
                Ok(report) => Ok(report.clone()),
                Err(error) => Err(AgentError::Storage(format!(
                    "checkpoint prepare failed: {error}"
                ))),
            };
            let _ = report_tx.send(for_barrier);
            let _sent = relay_op_tx
                .send(OperationCompletion {
                    operation: OperationResult {
                        run_id,
                        turn_id: turn_id.unwrap_or_default(),
                        task_id,
                        scope_id,
                        operation_id,
                        generation,
                        outcome: OperationOutcome::Completed,
                    },
                    kind: OpKind::Gc,
                    effect: None,
                    lease: None,
                    effect_id: None,
                    argument_digest: None,
                    attribution: None,
                    verification_call: None,
                    tool_identity: None,
                    value_completion_pending: false,
                    recovery_required: None,
                    directive: None,
                    disposition: ToolResultDisposition::PersistObservation,
                    context_ack: None,
                    maintenance: None,
                    materialization: None,
                    gc: Some(super::maintenance::GcOutcome::SafepointPrepareSettled(
                        report,
                    )),
                })
                .await;
        });
        // The parked prepare's handle becomes the report relay: blocking
        // barriers (shutdown, failure paths) still land the same report.
        prepare.handle = tokio::spawn(async move {
            report_rx.await.unwrap_or_else(|_| {
                Err(AgentError::Storage(
                    "checkpoint prepare relay dropped".into(),
                ))
            })
        });
        self.state.checkpoint_prepare = Some(prepare);
        self.state.gc_work = Some(super::maintenance::PendingGc::new(
            operation_id,
            super::maintenance::GcContinuation::SafepointCommit { content },
            relay,
        ));
        true
    }

    /// Barrier wait: explicit pause/suspend/completion/shutdown paths call
    /// this so they never report an outcome whose resume checkpoint is
    /// still in flight. A durable ack advances the sequence watermark; the
    /// frozen debt set was separated at freeze time, so debt accrued after
    /// that freeze survives. A failure restores the frozen set with every
    /// reason retained and an error return, so callers refuse to claim
    /// resumability.
    pub(super) async fn await_pending_checkpoint(&mut self) -> AgentResult<()> {
        // EXEC-7: a parked safe-point prepare lands here first — the barrier
        // semantics are unchanged (this returns only once the write is in
        // flight and drained, or the failure restored its debt), but the
        // wait happens on the prepare's own handle, not on the actor loop.
        if let Some(prepare) = self.state.checkpoint_prepare.take() {
            let report = match prepare.handle.await {
                Ok(report) => report,
                Err(join_error) => {
                    self.restore_checkpoint_debt(&prepare.captured_debt);
                    let error = AgentError::InvalidRequest(format!(
                        "checkpoint prepare task failed: {join_error}"
                    ));
                    self.emit_checkpoint_write_failed(error.to_string()).await;
                    return Err(error);
                }
            };
            self.land_safepoint_write(
                prepare.sequence,
                prepare.anchor_revision,
                prepare.captured_debt,
                Some(report),
            )
            .await;
        }
        if self.state.checkpoint_write.is_none() {
            return Ok(());
        }
        let in_flight = self
            .state
            .checkpoint_write
            .take()
            .expect("the in-flight write is present");
        match in_flight.handle.await {
            Ok(Ok((sequence, stored))) => {
                self.state.checkpoint_write_failed = false;
                self.state.durable_sequence =
                    Some(self.state.durable_sequence.unwrap_or(0).max(sequence));
                // Typed retirement: the frozen set left the live debt at
                // freeze time; debt accrued after that freeze stays.
                let anchor_revision = in_flight.anchor_revision;
                let capability_generation = in_flight.capability_generation;
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision: anchor_revision,
                        checksum: stored.checksum,
                        sequence,
                        capability_generation,
                    })
                    .await;
                Ok(())
            }
            Ok(Err(error)) => {
                self.restore_checkpoint_debt(&in_flight.captured_debt);
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
            Err(join_error) => {
                self.restore_checkpoint_debt(&in_flight.captured_debt);
                let error = AgentError::InvalidRequest(format!(
                    "checkpoint write task failed: {join_error}"
                ));
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
        }
    }

    /// Continuation across a segment is allowed only when durability is
    /// fully accounted for: no write in flight, no live debt that no
    /// snapshot has captured yet, no failed write without a subsequent
    /// durable one, and the required snapshot sequence actually landed
    /// durably. A failed or missing safe-point write fences
    /// `continue_active_task` before any model request and stays fenced
    /// until a retry succeeds.
    pub(super) async fn continuation_durability_gate(&mut self) -> AgentResult<()> {
        self.await_pending_checkpoint().await?;
        if let Some(required) = self.state.required_sequence {
            let durable = self.state.durable_sequence.unwrap_or(0);
            if durable < required {
                return Err(AgentError::RecoveryRequired(format!(
                    "the resume checkpoint for sequence {required} never landed durably \
                     (durable sequence {durable}); continuation is fenced until a retry succeeds"
                )));
            }
        }
        if !self.state.checkpoint_debt.is_empty()
            || self.state.checkpoint_write.is_some()
            || self.state.checkpoint_prepare.is_some()
        {
            return Err(AgentError::RecoveryRequired(
                "outstanding checkpoint debt has not been captured at a settled safe point \
                 yet; continuation is fenced until the next safe point lands"
                    .into(),
            ));
        }
        Ok(())
    }
}

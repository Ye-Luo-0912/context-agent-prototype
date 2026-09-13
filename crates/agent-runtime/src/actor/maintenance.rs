//! Context maintenance continuations on the actor's existing operation lane.

use super::*;

const MAINTENANCE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) enum TurnStartReply {
    Message(Reply<AgentResult<()>>),
    Continue(Reply<AgentResult<TaskId>>, TaskId),
    Work(
        Reply<AgentResult<crate::work::WorkSubmission>>,
        crate::work::WorkSubmission,
        crate::work::WorkSubmissionRecord,
    ),
}

pub(super) struct PendingTurnStart {
    pub(super) checkpoint: Option<serde_json::Value>,
    pub(super) directive: Option<crate::TaskDirective>,
    pub(super) reply: Option<TurnStartReply>,
}

pub(super) enum MaintenanceContinuation {
    UserInput(Box<PendingTurnStart>),
    BeforeModel,
    AfterTool {
        content: String,
    },
    AfterModel {
        evidence: Option<AssistantArtifactEvidence>,
    },
}

impl MaintenanceContinuation {
    fn trigger(&self) -> ContextMaintenanceTrigger {
        match self {
            Self::UserInput(_) => ContextMaintenanceTrigger::UserInput,
            Self::BeforeModel => ContextMaintenanceTrigger::BeforeModel,
            Self::AfterTool { .. } => ContextMaintenanceTrigger::AfterTool,
            Self::AfterModel { .. } => ContextMaintenanceTrigger::AfterModel,
        }
    }

    fn commit_phase(&self) -> Option<TurnCommitPhase> {
        match self {
            Self::AfterTool { .. } => Some(TurnCommitPhase::AfterToolMaintain),
            Self::AfterModel { .. } => Some(TurnCommitPhase::AfterModelMaintain),
            _ => None,
        }
    }
}

pub(super) struct PendingMaintenance {
    continuation: MaintenanceContinuation,
    task: JoinHandle<()>,
}

/// EXEC-1: the round tail parked while a spawned materialization runs. The
/// plan carries exactly the prepared locals the tail consumes; the join
/// handle lets cancellation prove the engine future stopped before any new
/// state is admitted.
pub(super) struct PendingMaterialization {
    pub(super) plan: super::model::ModelRoundPlan,
    task: JoinHandle<()>,
}

/// EXEC-1: bounded wait for an aborted materialization future. The engine's
/// `materialize` is a non-consuming preview — an aborted future releases the
/// gate/state locks and commits no consumption, so joining is all the
/// rollback there is; the timeout is the trustworthy-cancellation boundary.
const MATERIALIZATION_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

impl RuntimeActor {
    pub(super) fn set_turn_start_reply(&mut self, reply: TurnStartReply) {
        let Some(PendingMaintenance {
            continuation: MaintenanceContinuation::UserInput(start),
            ..
        }) = self.state.maintenance.as_mut()
        else {
            unreachable!("a newly prepared turn has a UserInput continuation");
        };
        start.reply = Some(reply);
    }

    fn reply_to_turn_start(&mut self, reply: Option<TurnStartReply>, result: AgentResult<()>) {
        match reply {
            Some(TurnStartReply::Message(reply)) => {
                let _ = reply.send(result);
            }
            Some(TurnStartReply::Continue(reply, task_id)) => {
                let _ = reply.send(result.map(|()| task_id));
            }
            Some(TurnStartReply::Work(reply, submission, record)) => {
                if result.is_ok() {
                    self.state.work_submissions.push_back(record);
                    while self.state.work_submissions.len()
                        > crate::work::MAX_PENDING_WORK_SUBMISSIONS
                    {
                        self.state.work_submissions.pop_front();
                    }
                }
                let _ = reply.send(result.map(|()| submission));
            }
            None => {}
        }
    }

    pub(super) fn spawn_maintenance(
        &mut self,
        continuation: MaintenanceContinuation,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let turn = self
            .state
            .turn
            .as_mut()
            .expect("maintenance belongs to a turn");
        debug_assert!(turn.op.is_none() && self.state.maintenance.is_none());
        let turn_id = turn.turn_id;
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let run_id = self.core.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        let cancel = CancellationToken::new();
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            kind: OpKind::Maintenance,
            scope_id: None,
            tool_identity: None,
            cancel: cancel.clone(),
            abort: None,
        });
        let context = self.services.context_engine();
        let trigger = continuation.trigger();
        // Episode compaction can await inside ingest, before maintain is
        // reached. Keep both calls under the same abort/join and rollback
        // boundary. A continuation has no new input transaction to ingest.
        let ingress = match &continuation {
            MaintenanceContinuation::UserInput(start) if start.checkpoint.is_some() => {
                Some(ContextIngress::UserMessage {
                    content: turn.turn_frame.user_message.clone(),
                })
            }
            _ => None,
        };
        let op_tx = op_tx.clone();
        let task = tokio::spawn(async move {
            let report = async {
                if let Some(ingress) = ingress {
                    context.ingest(ingress).await?;
                }
                context.maintain(trigger).await
            }
            .await;
            // A full completion channel cannot hold cancellation cleanup:
            // once the engine future ended, cancellation may drop this send.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = op_tx.send(OperationCompletion {
                    operation: OperationResult {
                        run_id, turn_id, task_id, scope_id, operation_id, generation,
                        outcome: OperationOutcome::Completed,
                    },
                    kind: OpKind::Maintenance,
                    effect: None, lease: None, effect_id: None, argument_digest: None,
                    attribution: None, verification_call: None, tool_identity: None,
                    value_completion_pending: false, recovery_required: None,
                    directive: None, disposition: ToolResultDisposition::PersistObservation,
                    context_ack: None, maintenance: Some(report),
                    materialization: None, gc: None,
                }) => {}
            }
        });
        turn.op.as_mut().unwrap().abort = Some(task.abort_handle());
        self.state.maintenance = Some(PendingMaintenance { continuation, task });
    }

    /// EXEC-1: run the round's context materialization as a spawned
    /// operation so the actor loop keeps receiving commands while the
    /// engine waits on its storage. Completion resumes the parked tail via
    /// `on_operation_completed`; cancellation aborts the future (the
    /// documented safe failure for a non-consuming preview) and joins it
    /// through `cancel_pending_materialization`.
    pub(super) fn spawn_materialization(
        &mut self,
        query: ContextQuery,
        plan: super::model::ModelRoundPlan,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let turn = self
            .state
            .turn
            .as_mut()
            .expect("materialization belongs to a turn");
        debug_assert!(turn.op.is_none() && self.state.materialization.is_none());
        let turn_id = turn.turn_id;
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let run_id = self.core.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        let cancel = CancellationToken::new();
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            kind: OpKind::Materialize,
            scope_id: None,
            tool_identity: None,
            cancel: cancel.clone(),
            abort: None,
        });
        let context = self.services.context_engine();
        let op_tx = op_tx.clone();
        let task = tokio::spawn(async move {
            let result = context.materialize(query).await;
            // A full completion channel cannot hold cancellation cleanup:
            // once the engine future ended, cancellation may drop this send.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = op_tx.send(OperationCompletion {
                    operation: OperationResult {
                        run_id, turn_id, task_id, scope_id, operation_id, generation,
                        outcome: OperationOutcome::Completed,
                    },
                    kind: OpKind::Materialize,
                    effect: None, lease: None, effect_id: None, argument_digest: None,
                    attribution: None, verification_call: None, tool_identity: None,
                    value_completion_pending: false, recovery_required: None,
                    directive: None, disposition: ToolResultDisposition::PersistObservation,
                    context_ack: None, maintenance: None,
                    materialization: Some(result), gc: None,
                }) => {}
            }
        });
        turn.op.as_mut().unwrap().abort = Some(task.abort_handle());
        self.state.materialization = Some(PendingMaterialization { plan, task });
    }

    /// EXEC-1: cancellation boundary for a parked materialization. The
    /// caller has already cancelled/aborted the operation; joining proves
    /// the engine future actually stopped. An unconfirmed join cannot claim
    /// a trustworthy cancellation, so it fences like an interrupted commit.
    pub(super) async fn cancel_pending_materialization(&mut self) -> AgentResult<()> {
        let Some(pending) = self.state.materialization.take() else {
            return Ok(());
        };
        let PendingMaterialization { plan: _, mut task } = pending;
        let stopped = match tokio::time::timeout(MATERIALIZATION_CLEANUP_TIMEOUT, &mut task).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => error.is_cancelled(),
            Err(_) => false,
        };
        if !stopped {
            let reason = "materialization cleanup was not confirmed".to_string();
            self.state.recovery_required = true;
            self.state.turn = None;
            let audit = self
                .core
                .emit_events_durable(vec![
                    RuntimeEvent::TurnCommitFailed {
                        phase: "materialize_cancel_cleanup".into(),
                        message: reason.clone(),
                    },
                    RuntimeEvent::RecoveryRequired,
                ])
                .await;
            return Err(AgentError::RecoveryRequired(match audit {
                Ok(()) => reason,
                Err(error) => format!("{reason}; recovery audit barrier failed: {error}"),
            }));
        }
        // Nothing to roll back: the preview never returned, so no
        // consumption was committed and no cost was incurred.
        Ok(())
    }

    pub(super) async fn continue_after_maintenance(
        &mut self,
        report: AgentResult<ContextMaintenanceReport>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let pending = self
            .state
            .maintenance
            .take()
            .expect("current maintenance has a continuation");
        // The completion is sent only after the engine future returned.
        match pending.continuation {
            MaintenanceContinuation::BeforeModel => {
                self.continue_model_operation_after_maintenance(op_tx, report)
                    .await;
            }
            MaintenanceContinuation::UserInput(start) => {
                self.finish_turn_start(*start, report, op_tx).await;
            }
            MaintenanceContinuation::AfterTool { content } => {
                let report = match report {
                    Ok(report) => report,
                    Err(error) => {
                        return self
                            .commit_failed(TurnCommitPhase::AfterToolMaintain, error)
                            .await;
                    }
                };
                if let Err(error) = self
                    .emit_context_maintained(ContextMaintenanceTrigger::AfterTool, report)
                    .await
                {
                    return self
                        .commit_failed(TurnCommitPhase::AfterToolMaintainedEvent, error)
                        .await;
                }
                self.finalize_assistant_message(content, op_tx).await;
            }
            MaintenanceContinuation::AfterModel { evidence } => {
                let report = match report {
                    Ok(report) => report,
                    Err(error) => {
                        return self
                            .commit_failed(TurnCommitPhase::AfterModelMaintain, error)
                            .await;
                    }
                };
                if let Err(error) = self
                    .emit_context_maintained(ContextMaintenanceTrigger::AfterModel, report)
                    .await
                {
                    return self
                        .commit_failed(TurnCommitPhase::AfterModelMaintainedEvent, error)
                        .await;
                }
                // EXEC-7: finalize parks on the turn-final GC; the queued
                // input drains at the real end of the commit tail
                // (`finish_turn_final_gc`).
                self.finalize_after_model(evidence).await;
            }
        }
    }

    async fn finish_turn_start(
        &mut self,
        start: PendingTurnStart,
        report: AgentResult<ContextMaintenanceReport>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let PendingTurnStart {
            checkpoint,
            directive,
            reply,
        } = start;
        let continuation = checkpoint.is_none();
        let report = match checkpoint {
            Some(checkpoint) => self.services.finish_user_message(checkpoint, report).await,
            None => report,
        };
        let result = async {
            let report = report.map_err(|error| self.context_transition_failed(error))?;
            let turn = self.state.turn.as_ref().expect("current turn start");
            let applied = turn.applied_input.clone().expect("prepared input");
            let content = turn.turn_frame.user_message.clone();
            if !continuation && let Err(error) = self.emit_user_input(applied).await {
                return Err(self.audit_gap_after_commit(error).await);
            }
            if let Err(error) = self
                .emit_context_maintained(ContextMaintenanceTrigger::UserInput, report)
                .await
            {
                return Err(self.audit_gap_after_commit(error).await);
            }
            if let Some(directive) = directive {
                self.state.tasks.apply_user_directive(&content, directive);
                // Admission advances the directive/verification basis. The
                // prepared turn held the old resume only while rollback was
                // still possible; model/tool work must use the committed one.
                let execution = self
                    .state
                    .task_id
                    .and_then(|id| self.state.tasks.get(id))
                    .expect("the admitted directive belongs to the active task")
                    .resume
                    .clone();
                self.state
                    .turn
                    .as_mut()
                    .expect("current turn start")
                    .execution = execution;
            }
            if continuation && let Some(task_id) = self.state.task_id {
                let anchor_revision = self
                    .state
                    .tasks
                    .get(task_id)
                    .map(|task| task.anchor.revision)
                    .unwrap_or_default();
                self.core
                    .emit_event(RuntimeEvent::TaskContinuationStarted {
                        task_id,
                        anchor_revision,
                    })
                    .await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = &result {
            self.state.turn = None;
            // Queued input has no waiting caller; its asynchronous failure
            // still needs a visible outcome.
            if reply.is_none() {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: crate::output::bound_error_message(format!(
                            "queued user input failed to start: {error}"
                        )),
                    })
                    .await;
            }
        }
        let succeeded = result.is_ok();
        self.reply_to_turn_start(reply, result);
        if succeeded {
            self.advance_turn(op_tx).await;
        }
        self.drain_queued_user_input(op_tx).await;
    }

    /// Called after the Core generation fence and abort are installed.
    /// Joining proves the old engine call cannot mutate a restored/new turn.
    pub(super) async fn cancel_pending_maintenance(&mut self) -> AgentResult<()> {
        let Some(pending) = self.state.maintenance.take() else {
            return Ok(());
        };
        let PendingMaintenance {
            continuation,
            mut task,
        } = pending;
        let stopped = match tokio::time::timeout(MAINTENANCE_CLEANUP_TIMEOUT, &mut task).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => error.is_cancelled(),
            Err(_) => false,
        };
        let phase = continuation.commit_phase();
        let mut failure = if stopped {
            None
        } else {
            Some("maintenance task cleanup was not confirmed".to_string())
        };
        if let MaintenanceContinuation::UserInput(start) = continuation {
            let start = *start;
            if stopped && let Some(checkpoint) = start.checkpoint {
                match tokio::time::timeout(
                    MAINTENANCE_CLEANUP_TIMEOUT,
                    self.services
                        .finish_user_message(checkpoint, Err(AgentError::Cancelled)),
                )
                .await
                {
                    Ok(Err(AgentError::Cancelled)) => {}
                    Ok(Err(error)) => failure = Some(error.to_string()),
                    _ => failure = Some("user-input rollback was not confirmed".into()),
                }
            }
            let reply_error = match &failure {
                Some(reason) => AgentError::RecoveryRequired(reason.clone()),
                None => AgentError::Cancelled,
            };
            self.reply_to_turn_start(start.reply, Err(reply_error));
            if let Some(turn) = self.state.turn.as_mut() {
                // New dialogue never crossed its applied audit boundary.
                if start.directive.is_some() {
                    turn.applied_input = None;
                }
            }
        }
        if phase.is_some() || failure.is_some() {
            // Post-effect maintenance is a mandatory commit step. Its
            // interruption cannot roll back effects or claim TurnCompleted.
            let phase = phase
                .map(TurnCommitPhase::as_str)
                .unwrap_or("maintenance_cancel_cleanup");
            let reason =
                failure.unwrap_or_else(|| format!("turn commit interrupted during {phase}"));
            self.state.recovery_required = true;
            self.state.turn = None;
            let audit = self
                .core
                .emit_events_durable(vec![
                    RuntimeEvent::TurnCommitFailed {
                        phase: phase.into(),
                        message: reason.clone(),
                    },
                    RuntimeEvent::RecoveryRequired,
                ])
                .await;
            return Err(AgentError::RecoveryRequired(match audit {
                Ok(()) => reason,
                Err(error) => format!("{reason}; recovery audit barrier failed: {error}"),
            }));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EXEC-7 (R2-08): boundary work on the existing operation lane. Full-GC
// passes, the post-completion storage boundary, and checkpoint-trigger
// maintenance run as spawned operations so a slow engine (a gated store in
// tests, a real compactor in production) never occupies the actor's command
// branch. Turn-scoped work cancels like maintenance: aborting the future is
// the engine-documented safe failure (eviction is reversible and the pass
// never committed). Idle boundary work is joined by shutdown; landed
// physical deletes are durable facts and are never claimed back.
// ---------------------------------------------------------------------------

/// The engine result one spawned `OpKind::Gc` operation carries home.
pub(super) enum GcOutcome {
    /// One full GC pass (turn-final boundary or an explicit collect).
    FullGc(AgentResult<agent_contracts::ContextGcReport>),
    /// The post-completion boundary: the full GC result and the storage
    /// boundary result, reported independently — a failed full pass never
    /// skipped the storage boundary, and neither rolls anything back.
    CompletionBoundary(
        AgentResult<agent_contracts::ContextGcReport>,
        AgentResult<agent_contracts::StorageGcReport>,
    ),
    /// Checkpoint-trigger maintenance ahead of an assembly resume.
    CheckpointMaintain(AgentResult<agent_contracts::ContextMaintenanceReport>),
    /// EXEC-7: a safe-point prepare task settled — the report rides home and
    /// the parked turn-commit tail (carried by the continuation) resumes
    /// after the prepare lands.
    SafepointPrepareSettled(AgentResult<agent_contracts::ContextMaintenanceReport>),
}

/// What to resume when a checkpoint-maintenance operation returns. The
/// assembly itself stays on the actor; only the slow engine call leaves the
/// command branch.
pub(super) enum CheckpointResume {
    /// The terminal commit's freeze half, parked with the whole prepared
    /// transaction. The runtime refuses new mutations while this is parked:
    /// the completion transaction must not apply over a moved task table.
    TerminalFreeze(Box<TerminalFreezePark>),
    /// A read-only capture (`Checkpoint` command): the reply is parked and
    /// answered by the resume, keeping status and other commands live
    /// during the maintenance.
    ReadOnlyCapture {
        reply: crate::actor::Reply<AgentResult<crate::checkpoint::RuntimeCheckpoint>>,
    },
}

/// Everything `commit_task_completion` needs once the terminal freeze's
/// checkpoint maintenance has run. The transaction is prepared but NOT
/// applied: a failure path rolls the context plane back and leaves the
/// TaskManager exactly as it was.
pub(super) struct TerminalFreezePark {
    pub(super) intent: CompletionIntent,
    pub(super) txn: crate::task::TaskTxn,
    pub(super) task_id: TaskId,
    pub(super) anchor_revision: u64,
    pub(super) event_summary: String,
    pub(super) prepared: crate::services::PreparedTaskCompletion,
    pub(super) next_focus_revision: u64,
    /// EXEC-8: the record's bounded evidence facts for the durable
    /// TaskCompleted event.
    pub(super) artifacts: Vec<String>,
    pub(super) final_output_digest: Option<String>,
    /// Freeze internals captured before the maintenance: the prospective
    /// terminal planes and the sequence/debt bookkeeping it owns.
    pub(super) terminal_tasks: crate::checkpoint::TaskManagerSnapshot,
    pub(super) terminal_focus_revision: u64,
    pub(super) sequence: u64,
    pub(super) prior_required_sequence: Option<u64>,
    pub(super) prior_debt: Vec<crate::checkpoint::CheckpointDebtReason>,
    pub(super) anchor: u64,
    pub(super) reply: Option<crate::actor::Reply<AgentResult<()>>>,
}

pub(super) struct PendingGc {
    pub(super) operation_id: OperationId,
    pub(super) continuation: GcContinuation,
    task: JoinHandle<()>,
}

impl PendingGc {
    pub(super) fn new(
        operation_id: OperationId,
        continuation: GcContinuation,
        task: JoinHandle<()>,
    ) -> Self {
        Self {
            operation_id,
            continuation,
            task,
        }
    }
}

pub(super) enum GcContinuation {
    /// The turn-final full GC between the last model round and the commit
    /// barrier. The turn's `InFlightOp` fences it; cancellation aborts it.
    TurnFinal {
        evidence: Option<super::AssistantArtifactEvidence>,
    },
    /// A trusted directive asked for an explicit collect at operation-commit
    /// time. The turn's `InFlightOp` fences it; cancellation aborts it. The
    /// deferred model decision round resumes when the pass lands.
    ExplicitCollect,
    /// EXEC-7: the turn-commit tail parked behind a safe-point prepare (the
    /// TurnCompleted barrier must follow the resume checkpoint, but the
    /// actor's command branch must stay free while the engine runs). The
    /// relay completion carries the maintenance report; landing the prepare
    /// resumes this tail.
    SafepointCommit {
        /// The turn's final assistant message for the tail's evidence write.
        content: String,
    },
    /// Post-completion boundary work. No turn owns it; cancellation never
    /// targets it, shutdown joins it bounded, and landed deletes stay facts.
    CompletionBoundary,
    /// Checkpoint-trigger maintenance before an assembly resumes.
    CheckpointMaintain(CheckpointResume),
}

impl PendingGc {
    pub(super) fn is_commit_in_flight(&self) -> bool {
        matches!(
            self.continuation,
            GcContinuation::CheckpointMaintain(CheckpointResume::TerminalFreeze(_))
        )
    }

    /// True when the completion belongs to this parked work.
    fn owns(&self, operation_id: OperationId) -> bool {
        // The parked slot is the single flight: a cancel takes the slot, so
        // a late completion finds nothing to resume and is dropped — a GC
        // pass that never committed has nothing to roll back.
        self.operation_id == operation_id
    }
}

/// Which engine call a spawned `OpKind::Gc` operation performs.
enum GcEngineWork {
    FullGc,
    CompletionBoundary,
    CheckpointMaintain,
}

impl RuntimeActor {
    /// EXEC-7: spawn one boundary operation. Turn-scoped continuations take
    /// the turn's `InFlightOp` (kind `Gc`) so the generic cancellation path
    /// sees them; idle continuations run without a turn. Exactly one boundary
    /// operation is parked at a time.
    async fn spawn_gc_op(
        &mut self,
        continuation: GcContinuation,
        work: GcEngineWork,
    ) -> Result<(), (GcContinuation, AgentError)> {
        // EXEC-10 (R3-10): the boundary lane is a SINGLE slot with an
        // explicit vacancy check. Overwriting a parked entry would drop its
        // reply sender and JoinHandle without ever settling them — the
        // second request receives a deterministic busy refusal with its
        // continuation handed back, so every parked entry keeps a
        // determinate outcome and the refused resume is never lost.
        if self.state.gc_work.is_some() {
            return Err((
                continuation,
                AgentError::InvalidRequest(
                    "a boundary operation (checkpoint maintenance / gc pass) is already in \
                     flight; retry when it settles"
                        .into(),
                ),
            ));
        }
        let Some(op_tx) = self.op_tx() else {
            // No completion loop owns this actor (direct-call construction):
            // run the engine call inline and settle it synchronously — the
            // same behavior the pre-EXEC-7 inline waits had.
            let context = self.services.context_engine();
            let services = self.services.clone();
            let outcome = run_gc_engine_work(context, services, work).await;
            let operation_id = OperationId::new();
            let generation = self.state.generation;
            let run_id = self.core.run_id();
            let task_id = self.state.task_id;
            let scope_id = self.state.scope_id;
            let turn_id = self.state.turn.as_ref().map(|turn| turn.turn_id);
            let (dummy_tx, _dummy_rx) = mpsc::channel(1);
            // The dispatch fences against the parked slot — park a completed
            // placeholder so the inline completion is owned and dispatched.
            self.state.gc_work = Some(PendingGc {
                operation_id,
                continuation,
                task: tokio::spawn(async {}),
            });
            self.continue_after_gc_work(
                OperationCompletion {
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
                    gc: Some(outcome),
                },
                &dummy_tx,
            )
            .await;
            return Ok(());
        };
        let turn_scoped = matches!(continuation, GcContinuation::TurnFinal { .. });
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let run_id = self.core.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        let turn_id = self.state.turn.as_ref().map(|turn| turn.turn_id);
        let cancel = CancellationToken::new();
        if turn_scoped {
            let turn = self
                .state
                .turn
                .as_mut()
                .expect("turn-scoped gc work belongs to a turn");
            turn.op = Some(InFlightOp {
                operation_id,
                turn_id: turn.turn_id,
                generation,
                kind: OpKind::Gc,
                scope_id: None,
                tool_identity: None,
                cancel: cancel.clone(),
                abort: None,
            });
        }
        let context = self.services.context_engine();
        let services = self.services.clone();
        let task = tokio::spawn(async move {
            let outcome = run_gc_engine_work(context, services, work).await;
            // A full completion channel cannot hold cancellation cleanup:
            // once the engine future ended, cancellation may drop this send.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = op_tx.send(OperationCompletion {
                    operation: OperationResult {
                        run_id, turn_id: turn_id.unwrap_or_default(), task_id, scope_id, operation_id, generation,
                        outcome: OperationOutcome::Completed,
                    },
                    kind: OpKind::Gc,
                    effect: None, lease: None, effect_id: None, argument_digest: None,
                    attribution: None, verification_call: None, tool_identity: None,
                    value_completion_pending: false, recovery_required: None,
                    directive: None, disposition: ToolResultDisposition::PersistObservation,
                    context_ack: None, maintenance: None, materialization: None,
                    gc: Some(outcome),
                }) => {}
            }
        });
        if turn_scoped
            && let Some(operation) = self.state.turn.as_mut().and_then(|turn| turn.op.as_mut())
        {
            operation.abort = Some(task.abort_handle());
        }
        self.state.gc_work = Some(PendingGc {
            operation_id,
            continuation,
            task,
        });
        Ok(())
    }

    /// CTX-8 接线 (R3-08): is the engine's store-outage backpressure
    /// currently active? Drives the new-body-production limit while every
    /// control/query channel stays live.
    pub(super) fn store_backpressure_active(&self) -> bool {
        self.state
            .store_backpressure
            .as_ref()
            .is_some_and(|bp| bp.active)
    }

    /// EXEC-7: park the turn-final full GC. The commit tail resumes from the
    /// completion; a cancelled pass dies with the turn (the pass never
    /// committed, so there is nothing to roll back).
    pub(super) async fn begin_turn_final_gc(
        &mut self,
        evidence: Option<super::AssistantArtifactEvidence>,
    ) {
        if self
            .spawn_gc_op(GcContinuation::TurnFinal { evidence }, GcEngineWork::FullGc)
            .await
            .is_err()
        {
            // Practically unreachable (the slot is free at the turn-final
            // boundary), but the pass is mandatory: run it inline rather
            // than silently skipping the turn's only full GC. The parked
            // evidence went with the refused spawn, so the resumed tail has
            // none — the pass itself still lands and is audited.
            let report = self.services.context_engine().gc().await;
            self.finish_turn_final_gc(report, None).await;
        }
    }

    /// EXEC-7: park an explicit collect directive's full pass. The deferred
    /// model decision round resumes when the pass lands.
    pub(super) async fn begin_explicit_collect(&mut self) {
        if let Err(error) = self
            .spawn_gc_op(GcContinuation::ExplicitCollect, GcEngineWork::FullGc)
            .await
        {
            // Same unreachable-in-practice fallback as the turn-final pass:
            // the collect's audit must not be silently dropped.
            let _ = error;
            let context = self.services.context_engine();
            let report = context.gc().await;
            self.finish_explicit_collect(report).await;
        }
    }

    /// EXEC-7: park the post-completion boundary (full GC + root enumeration
    /// + storage GC). Post-commit: the outcome only ever becomes events.
    pub(super) async fn begin_completion_boundary(&mut self) {
        if let Err(error) = self
            .spawn_gc_op(
                GcContinuation::CompletionBoundary,
                GcEngineWork::CompletionBoundary,
            )
            .await
        {
            // The post-commit boundary is events-only; a slot collision
            // (practically unreachable) downgrades to running the passes
            // inline rather than dropping the audit.
            let _ = error;
            let context = self.services.context_engine();
            let gc = context.gc().await;
            let (roots, roots_complete) =
                collect_checkpoint_recovery_roots_for(&self.services).await;
            let storage = self
                .services
                .context_storage_gc_protecting(&roots, roots_complete)
                .await;
            self.finish_completion_boundary(gc, storage).await;
        }
    }

    /// EXEC-7: park checkpoint maintenance ahead of one assembly resume.
    /// A Core recovery fence keeps the assembly pure — no maintenance claim —
    /// exactly like the inline path it replaces.
    /// EXEC-7: park checkpoint maintenance ahead of one assembly resume.
    /// A Core recovery fence keeps the assembly pure — no maintenance claim —
    /// exactly like the inline path it replaces.
    ///
    /// EXEC-10 (R3-10): the lane is single-slot. When another boundary op
    /// still occupies it (e.g. the previous completion's storage boundary
    /// under a burst of completions), the maintenance runs INLINE and lands
    /// immediately — the reply/transaction stays determinate instead of the
    /// parked entry being overwritten. The inline wait is bounded by the
    /// engine call, not the store.
    pub(super) async fn begin_checkpoint_maintain(&mut self, resume: CheckpointResume) {
        let fenced = matches!(
            self.core.recovery_status(),
            agent_contracts::AuthorityRecoveryStatus::RecoveryRequired { .. }
        );
        let run_inline = fenced || self.state.gc_work.is_some();
        if run_inline {
            // Fenced Core: no maintenance claim, pure snapshot — or the lane
            // is busy: the maintenance runs here, bounded by the engine call.
            let report = if fenced {
                None
            } else {
                Some(
                    self.services
                        .context_engine()
                        .maintain(ContextMaintenanceTrigger::Checkpoint)
                        .await,
                )
            };
            self.finish_checkpoint_maintain(resume, report).await;
            return;
        }
        if let Err((continuation, error)) = self
            .spawn_gc_op(
                GcContinuation::CheckpointMaintain(resume),
                GcEngineWork::CheckpointMaintain,
            )
            .await
        {
            // Raced (single actor loop — practically unreachable): the
            // refused spawn handed the resume back; land it inline so the
            // resume always has a report.
            let _ = error;
            if let GcContinuation::CheckpointMaintain(resume) = continuation {
                let report = self
                    .services
                    .context_engine()
                    .maintain(ContextMaintenanceTrigger::Checkpoint)
                    .await;
                self.finish_checkpoint_maintain(resume, Some(report)).await;
            }
        }
    }

    /// Dispatch a finished `OpKind::Gc` completion against its parked
    /// continuation. Anything without a live parked owner is dropped: a
    /// cancelled boundary pass never committed, so dropping IS the rollback
    /// (and the storage boundary never claims landed deletes back).
    pub(super) fn continue_after_gc_work<'a>(
        &'a mut self,
        completion: OperationCompletion,
        op_tx: &'a mpsc::Sender<OperationCompletion>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.continue_after_gc_work_inner(completion, op_tx))
    }

    async fn continue_after_gc_work_inner(
        &mut self,
        completion: OperationCompletion,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let Some(outcome) = completion.gc else {
            return;
        };
        let owns = self
            .state
            .gc_work
            .as_ref()
            .is_some_and(|pending| pending.owns(completion.operation.operation_id));
        if !owns {
            return;
        }
        let pending = self.state.gc_work.take().expect("ownership checked above");
        let PendingGc {
            operation_id: _,
            continuation,
            task,
        } = pending;
        let dispatch = async {
            match (continuation, outcome) {
                (GcContinuation::TurnFinal { evidence }, GcOutcome::FullGc(report)) => {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.op = None;
                    }
                    self.finish_turn_final_gc(report, evidence).await;
                }
                (
                    GcContinuation::CompletionBoundary,
                    GcOutcome::CompletionBoundary(gc, storage),
                ) => {
                    self.finish_completion_boundary(gc, storage).await;
                }
                (
                    GcContinuation::CheckpointMaintain(resume),
                    GcOutcome::CheckpointMaintain(report),
                ) => {
                    self.finish_checkpoint_maintain(resume, Some(report)).await;
                }
                (GcContinuation::ExplicitCollect, GcOutcome::FullGc(report)) => {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.op = None;
                    }
                    self.finish_explicit_collect(report).await;
                    // The collect landed; the deferred model decision round
                    // now assembles — with the eviction the model asked for
                    // in place.
                    self.spawn_model_operation(op_tx).await;
                }
                (
                    GcContinuation::SafepointCommit { content },
                    GcOutcome::SafepointPrepareSettled(report),
                ) => {
                    // Release the turn's operation slot: the tail (and any
                    // maintenance it spawns) needs it free.
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.op = None;
                    }
                    // Land the parked prepare with its own frozen
                    // bookkeeping, then resume the turn-commit tail that the
                    // TurnCompleted ordering barrier had to defer.
                    if let Some(prepare) = self.state.checkpoint_prepare.take() {
                        let report = match report {
                            Ok(report) => Some(Ok(report)),
                            Err(error) => {
                                self.restore_checkpoint_debt(&prepare.captured_debt);
                                self.emit_checkpoint_write_failed(error.to_string()).await;
                                Some(Err(error))
                            }
                        };
                        let _ = self
                            .land_safepoint_write(
                                prepare.sequence,
                                prepare.anchor_revision,
                                prepare.captured_debt,
                                report,
                            )
                            .await;
                    }
                    self.finalize_turn_tail(content, op_tx).await;
                }
                (continuation, outcome) => {
                    // A continuation/outcome pairing that cannot happen given
                    // the spawn sites — refuse to guess; park nothing.
                    let _ = (continuation, outcome);
                }
            }
        };
        dispatch.await;
        // The spawned operation's future produced its completion, but its
        // task may still be returning — its captured services (and whatever
        // they hold) must be dropped before a caller can depend on those
        // resources being released (shutdown joins, journal locks).
        let _ = task.await;
    }

    /// Cancellation boundary for turn-scoped boundary work. The caller
    /// already cancelled/aborted the operation; joining proves the engine
    /// future actually stopped. An unconfirmed join cannot claim a
    /// trustworthy cancellation, so it fences like an interrupted commit.
    pub(super) async fn cancel_pending_gc_work(&mut self) -> AgentResult<()> {
        let Some(pending) = self.state.gc_work.take() else {
            return Ok(());
        };
        if let GcContinuation::SafepointCommit { .. } = pending.continuation {
            // The parked turn-commit tail dies with the cancelled turn, but
            // the relay is NEVER aborted: it still carries the safe-point
            // report home (through the completion channel, whose stale
            // delivery the ownership check drops), and the parked prepare
            // lands through the ordinary barriers. Detach silently.
            return Ok(());
        }
        if !matches!(
            pending.continuation,
            GcContinuation::TurnFinal { .. } | GcContinuation::ExplicitCollect
        ) {
            // Idle boundary work is not cancelled with the turn: it has no
            // turn to revoke, its outcome is events only, and landed
            // physical deletes are durable facts. Shutdown joins it.
            self.state.gc_work = Some(pending);
            return Ok(());
        }
        let PendingGc {
            operation_id: _,
            continuation,
            mut task,
        } = pending;
        let stopped = match tokio::time::timeout(MAINTENANCE_CLEANUP_TIMEOUT, &mut task).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => error.is_cancelled(),
            Err(_) => false,
        };
        if !stopped {
            let reason = "boundary gc cleanup was not confirmed".to_string();
            self.state.recovery_required = true;
            self.state.turn = None;
            let audit = self
                .core
                .emit_events_durable(vec![
                    RuntimeEvent::TurnCommitFailed {
                        phase: "gc_cancel_cleanup".into(),
                        message: reason.clone(),
                    },
                    RuntimeEvent::RecoveryRequired,
                ])
                .await;
            return Err(AgentError::RecoveryRequired(match audit {
                Ok(()) => reason,
                Err(error) => format!("{reason}; recovery audit barrier failed: {error}"),
            }));
        }
        // The pass never committed: eviction is the engine's reversible
        // residency machine and nothing was consumed, so the dying turn's
        // commit tail simply does not run.
        let _ = continuation;
        Ok(())
    }

    /// Shutdown: one bounded drain for idle boundary work. A boundary that
    /// cannot finish inside the shutdown window is aborted, and its landed
    /// physical deletes remain the durable facts they already are.
    pub(super) async fn drain_gc_work_at_shutdown(
        &mut self,
        op_rx: &mut mpsc::Receiver<OperationCompletion>,
        op_tx: &mpsc::Sender<OperationCompletion>,
        proof_tx: &mpsc::Sender<turn::DeferredProofRefresh>,
    ) -> AgentResult<()> {
        if self.state.gc_work.is_none() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_OPERATION_DRAIN_TIMEOUT;
        // Drain EVERY boundary operation, including ones spawned while a
        // drained one resumed (a settling terminal commit spawns the
        // completion storage boundary behind it).
        while self.state.gc_work.is_some() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, op_rx.recv()).await {
                Ok(Some(completion)) => {
                    self.on_operation_completed(completion, op_tx, proof_tx)
                        .await;
                }
                Ok(None) | Err(_) => break,
            }
        }
        if let Some(pending) = self.state.gc_work.take() {
            pending.task.abort();
            let message = format!(
                "boundary operation {} did not settle before the shutdown deadline; its pass was aborted and landed deletes remain durable facts",
                pending.operation_id
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: message.clone(),
                })
                .await;
            return Err(AgentError::RecoveryRequired(message));
        }
        Ok(())
    }
}

/// The engine call one boundary operation performs, shared by the spawned
/// task and the inline fallback.
async fn run_gc_engine_work(
    context: Arc<dyn agent_contracts::ContextEngine>,
    services: Arc<RuntimeServices>,
    work: GcEngineWork,
) -> GcOutcome {
    match work {
        GcEngineWork::FullGc => GcOutcome::FullGc(context.gc().await),
        GcEngineWork::CheckpointMaintain => GcOutcome::CheckpointMaintain(
            context
                .maintain(ContextMaintenanceTrigger::Checkpoint)
                .await,
        ),
        GcEngineWork::CompletionBoundary => {
            // The full pass and the storage boundary report independently:
            // a failed full pass never skipped the storage boundary.
            let gc = context.gc().await;
            let (roots, roots_complete) = collect_checkpoint_recovery_roots_for(&services).await;
            let storage = services
                .context_storage_gc_protecting(&roots, roots_complete)
                .await;
            GcOutcome::CompletionBoundary(gc, storage)
        }
    }
}

/// The retained-checkpoint recovery-root enumeration, usable from the
/// spawned boundary task (it needs only the services, not actor state).
/// Decodes and validates every candidate before anything is read from it,
/// exactly like the actor-side collector this shares a contract with.
/// EXEC-9 (R3-01): the single shared implementation — the actor-side
/// collector delegates here, so in-process and spawned-boundary callers
/// (and the service engine behind the adapter) always answer identically.
pub(super) async fn collect_checkpoint_recovery_roots_for(
    services: &RuntimeServices,
) -> (Vec<agent_contracts::ContextItemId>, bool) {
    let Some(workspace) = services.artifact_workspace() else {
        return (Vec::new(), true);
    };
    let store = crate::CheckpointStore::new(workspace.state_dir().join("checkpoints"));
    let Ok(listed) = store
        .list(crate::checkpoint::MAX_CHECKPOINT_LIST_ROWS)
        .await
    else {
        return (Vec::new(), false);
    };
    let mut complete = listed.len() < crate::checkpoint::MAX_CHECKPOINT_LIST_ROWS;
    let mut roots = std::collections::HashSet::new();
    for row in listed {
        let payload = match store.load_verified(&row.artifact).await {
            Ok(payload) => payload,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let Ok(checkpoint) = crate::checkpoint::decode_checkpoint_bytes(&payload) else {
            complete = false;
            continue;
        };
        // EXEC-9 (R3-01): an enumeration failure (the engine behind a
        // process boundary cannot parse or reach its own payload) marks the
        // window INCOMPLETE — deletion defers — instead of an empty set
        // that would read as "nothing retained".
        match services
            .context_checkpoint_recovery_item_ids(&checkpoint.context)
            .await
        {
            Ok(ids) => roots.extend(ids),
            Err(_) => complete = false,
        }
    }
    (roots.into_iter().collect(), complete)
}

impl RuntimeActor {
    /// EXEC-7: the checkpoint-maintenance resume. The report (or its
    /// failure) is applied per parked resume kind; a fenced spawn already
    /// resumed inline with no report.
    /// Boxed like the operation pump: the resume chain can recurse back
    /// into boundary spawns, and the actor loop's future must stay sized.
    pub(super) fn finish_checkpoint_maintain<'a>(
        &'a mut self,
        resume: CheckpointResume,
        report: Option<AgentResult<agent_contracts::ContextMaintenanceReport>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.finish_checkpoint_maintain_inner(resume, report))
    }

    async fn finish_checkpoint_maintain_inner(
        &mut self,
        resume: CheckpointResume,
        report: Option<AgentResult<agent_contracts::ContextMaintenanceReport>>,
    ) {
        match resume {
            CheckpointResume::TerminalFreeze(park) => {
                let _ = self.finish_terminal_freeze(*park, report).await;
            }
            CheckpointResume::ReadOnlyCapture { reply } => {
                let outcome = match report {
                    Some(Ok(maintain_report)) => {
                        if let Err(error) = self
                            .emit_context_maintained(
                                ContextMaintenanceTrigger::Checkpoint,
                                maintain_report,
                            )
                            .await
                        {
                            Err(error)
                        } else {
                            self.capture_checkpoint().await
                        }
                    }
                    Some(Err(error)) => Err(error),
                    None => self.capture_checkpoint().await,
                };
                let _ = reply.send(outcome);
            }
        }
    }

    /// EXEC-7: a read-only checkpoint capture parks its reply behind the
    /// spawned maintenance, so monitoring/status traffic stays live during
    /// a slow engine while the reply still carries the same authoritative
    /// snapshot the inline path produced.
    pub(super) async fn begin_read_only_capture(
        &mut self,
        reply: crate::actor::Reply<AgentResult<RuntimeCheckpoint>>,
    ) {
        self.begin_checkpoint_maintain(CheckpointResume::ReadOnlyCapture { reply })
            .await;
    }
}

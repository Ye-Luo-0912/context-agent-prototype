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
        let op_tx = op_tx.clone();
        let task = tokio::spawn(async move {
            let report = context.maintain(trigger).await;
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
                }) => {}
            }
        });
        turn.op.as_mut().unwrap().abort = Some(task.abort_handle());
        self.state.maintenance = Some(PendingMaintenance { continuation, task });
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
                self.finalize_after_model(evidence).await;
                self.drain_queued_user_input(op_tx).await;
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

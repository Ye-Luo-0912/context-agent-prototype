use super::*;

impl RuntimeActor {
    /// Commit the turn start: user message into the long-term context, then
    /// spawn the first model operation.
    pub(super) async fn start_turn(
        &mut self,
        content: String,
        reply: Reply<AgentResult<()>>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        if self.state.recovery_required {
            let _ = reply.send(Err(AgentError::RecoveryRequired(
                "runtime recovery is required before normal mutation may continue".into(),
            )));
            return;
        }
        if let Some(operation_id) = self.state.pending_tool_cleanup {
            let error = AgentError::InvalidRequest(format!(
                "agent is finishing explicit cleanup for cancelled tool operation {operation_id}"
            ));
            let _ = self.record_rejected_user_dialogue(&content).await;
            let _ = reply.send(Err(error));
            return;
        }
        if content.trim().is_empty() {
            let _ = reply.send(Ok(()));
            return;
        }
        if self.state.turn.is_some() {
            let _ = reply.send(self.queue_user_dialogue(content).await);
            return;
        }

        let input_id = RuntimeInputId::new();
        let persist = match self.persist_user_input_body(&content).await {
            Ok(stored) => stored,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let input = RuntimeInputEnvelope::user_dialogue(
            content.clone(),
            Some(input_id),
            self.state.task_id,
            None,
            persist.0,
            persist.1,
        );
        let _ = reply.send(self.begin_applied_turn(content, input, op_tx).await);
    }

    pub(super) async fn begin_applied_turn(
        &mut self,
        content: String,
        mut input: RuntimeInputEnvelope,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) -> AgentResult<()> {
        // Fence the new turn before an implicit focus or user-message write
        // becomes visible. If a later step fails, the unused epoch is safe;
        // an accepted turn can never run under an older Core authority.
        self.bump_generation()?;

        // The first message with no active task auto-creates one: a task is
        // the long-lived entity and the engine must never mint a TaskId, so
        // this is the single place an implicit task can be born. The focus
        // change lands before the message is ingested, exactly like an
        // explicit `/focus`.
        if self.state.tasks.active().is_none() {
            let next_focus_revision = self.next_focus_revision()?;
            let (txn, task_id) = self.state.tasks.prepare_create(content.trim());
            match self.services.set_focus(task_id, content.clone()).await {
                Err(error) => return Err(self.context_transition_failed(error)),
                Ok(report) => {
                    self.state.tasks.commit(txn);
                    self.state.task_id = Some(task_id);
                    self.state
                        .task_requirement_high_water
                        .entry(task_id)
                        .or_insert(0);
                    self.state.focus_revision = next_focus_revision;
                    self.publish_context_transition(
                        RuntimeEvent::FocusChanged {
                            task_id,
                            goal: content.clone(),
                        },
                        ContextMaintenanceTrigger::FocusChanged,
                        report,
                    )
                    .await?;
                }
            }
        }

        let turn_id = TurnId::new();
        if input.body_ref.is_none() {
            let (body_ref, digest) = self.persist_user_input_body(&content).await?;
            input.body_ref = body_ref;
            input.digest = digest;
        }
        input.turn_id = Some(turn_id);
        input.task_id = self.state.task_id;
        input.lifecycle = InputLifecycle::Applied;
        // ingest 成功后再发 Applied 事件，避免日志里有 Accepted 而上下文没有正文。
        self.services
            .context_ingest(ContextIngress::UserMessage {
                content: content.clone(),
            })
            .await?;
        let applied = input.with_lifecycle(InputLifecycle::Applied);
        self.emit_user_input(applied.clone()).await?;
        let report = self
            .services
            .context_maintain(ContextMaintenanceTrigger::UserInput)
            .await?;
        self.emit_context_maintained(ContextMaintenanceTrigger::UserInput, report)
            .await?;

        self.state.tasks.on_user_turn(&content);

        // A new turn has no active call from a previous turn: the
        // active-call policy only pins tools while the turn that issued
        // them still consumes their results.
        self.state.active_tool = None;
        self.state.discovery_budget.reset();
        let execution = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.resume.clone())
            .unwrap_or_default();
        self.state.turn = Some(ActiveTurn {
            turn_id,
            turn_frame: TurnFrame::new(content),
            model_round: 0,
            pending_tools: VecDeque::new(),
            tool_surface: None,
            turn_state: TurnState::Running,
            op: None,
            execution,
            edit_attempts: Vec::new(),
            launch_failures: Vec::new(),
            protocol_bodies: crate::execution::body_cache::ProtocolBodyCache::default(),
            round_snapshot: None,
            pending_completion: None,
            applied_input: Some(applied),
            input_consumed: false,
            structurally_empty_retries: 0,
            pending_scope_closes: VecDeque::new(),
        });
        self.advance_turn(op_tx).await;
        Ok(())
    }

    /// Spawn the next operation the turn state says should run: a pending
    /// tool call, or the next model round. No-op while one is in flight.
    pub(super) async fn advance_turn(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
        enum Action {
            Model,
            Tool(ToolCall),
        }
        let action = {
            let Some(turn) = self.state.turn.as_mut() else {
                return;
            };
            if turn.op.is_some() {
                return;
            }
            if let Some(call) = turn.pending_tools.pop_front() {
                Some(Action::Tool(call))
            } else {
                Some(Action::Model)
            }
        };
        let Some(action) = action else {
            return;
        };
        match action {
            Action::Model => self.spawn_next_model_or_end(op_tx).await,
            Action::Tool(call) => self.spawn_tool_operation(call, op_tx).await,
        }
    }

    pub(super) async fn spawn_next_model_or_end(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let over_budget = self.state.turn.as_ref().is_some_and(|turn| {
            turn.op.is_none() && turn.model_round >= self.services.max_tool_rounds()
        });
        if over_budget {
            let message = format!(
                "tool round budget exhausted after {} rounds",
                self.services.max_tool_rounds()
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Warning {
                    message: message.clone(),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::Error { message }).await;
            // Deliberate refusal (round budget), not a fault: settle the
            // applied input and drop the turn without fencing.
            self.settle_aborted_turn().await;
            return;
        }
        self.spawn_model_operation(op_tx).await;
    }
    /// Execute a runtime directive a tool attached to its output. Runs at
    /// operation-commit time — after any staged effect, before the result
    /// enters the turn frame — so a "manual collect now" is actually now,
    /// and a hint/lease/tag lands before the next model round, not at turn
    /// end. Only trusted tools and `runtime:context-control` capabilities
    /// can produce directives (the dispatcher enforces that); here they are
    /// simply routed to the engine.
    pub(super) async fn execute_directive(&mut self, directive: RuntimeDirective) {
        match directive {
            RuntimeDirective::Context(agent_contracts::ContextAction::Collect) => {
                // GC 前把当前活跃任务的 anchor 根声明投影给引擎：声明指向
                // 的条目在本次 pass 中受保护/召回，store 声明保护留存。
                // 推送失败不阻塞 collect——引擎仍按已推送的根集运行。
                // 空投影跳过（collect 本身不是 ingest directive）。
                self.push_gc_projections(false).await;
                match self.services.context_gc().await {
                    Ok(report) => {
                        if let Err(error) = self
                            .core
                            .emit_event(RuntimeEvent::ContextGc { report })
                            .await
                        {
                            // The GC state change landed but its audit
                            // event did not: surface it instead of letting
                            // the state silently outrun its journal event.
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::Error {
                                    message: error.to_string(),
                                })
                                .await;
                        }
                    }
                    Err(error) => {
                        // A failed explicit collect is not silent: the model
                        // asked for a pass and the engine refused it.
                        let _ = self
                            .core
                            .emit_event(RuntimeEvent::Error {
                                message: error.to_string(),
                            })
                            .await;
                    }
                }
            }
            RuntimeDirective::Context(other) => {
                if let Err(error) = self
                    .services
                    .context_ingest(ContextIngress::ContextDirective { action: other })
                    .await
                {
                    // A quota refused the directive (keep_alive / lease
                    // caps): the model believes it was granted, so surface
                    // the refusal.
                    let _ = self
                        .core
                        .emit_warning(format!("context directive refused: {error}"))
                        .await;
                }
            }
            RuntimeDirective::CompleteTask(proposal) => {
                // Validated and stored on the turn; the CTX-10 transaction
                // runs at the turn's safe point (after the turn commits),
                // never mid-operation, so the completion cannot race an
                // in-flight tool or model call.
                if let Err(error) = self.accept_completion_proposal(proposal) {
                    let _ = self
                        .core
                        .emit_warning(format!("completion proposal refused: {error}"))
                        .await;
                }
            }
        }
    }

    /// Validate and accept a structured completion proposal from
    /// `task.complete`. It is stored on the turn and committed at the
    /// turn's safe point; a later proposal replaces an earlier one. The
    /// model-facing tool result already told the model the proposal was
    /// submitted — the refusal path here only fires for malformed input.
    pub(super) fn accept_completion_proposal(
        &mut self,
        proposal: CompletionProposal,
    ) -> AgentResult<()> {
        validate_completion_proposal(
            &proposal,
            self.services.artifact_workspace(),
            self.core.run_id(),
        )?;
        let Some(turn) = self.state.turn.as_ref() else {
            return Err(AgentError::InvalidRequest(
                "no active turn to complete".into(),
            ));
        };
        completion_from_execution(&turn.execution)?;
        let Some(turn) = self.state.turn.as_mut() else {
            return Err(AgentError::InvalidRequest(
                "no active turn to complete".into(),
            ));
        };
        turn.pending_completion = Some(proposal);
        Ok(())
    }

    /// Commit the active task's typed CompletionRecord — the CTX-10
    /// transaction: prepare the record, run the engine's focus/context
    /// transition, commit the task flip, publish `TaskCompleted`, then one
    /// full GC pass so the completed task's records leave the resident
    /// heap (durable retention; a GC failure after the commit is surfaced,
    /// never allowed to undo the outcome). Shared by the `/done` command
    /// and the model's `task.complete` proposal.
    pub(super) async fn commit_completion(
        &mut self,
        summary: String,
        artifacts: Vec<String>,
        next_focus_revision: u64,
    ) -> AgentResult<()> {
        let active_task = self
            .state
            .tasks
            .active()
            .ok_or_else(|| AgentError::InvalidRequest("no active task to complete".into()))?;

        // Revalidate at the commit safe point: acceptance and commit are
        // separated by the rest of the turn, so a referenced file may have
        // disappeared or changed type in between. Raw runtime evidence has
        // priority, then proposal refs retain their declared order. Canonical
        // locators make alias deduplication deterministic, and the persisted
        // record can never exceed the contract cap after adding raw evidence.
        let workspace = self.services.artifact_workspace().cloned();
        let raw_evidence = self
            .state
            .last_assistant_artifact
            .as_ref()
            .filter(|evidence| evidence.task_id == active_task)
            .cloned();
        let mut merged_artifacts = Vec::with_capacity(MAX_COMPLETION_ARTIFACTS);
        let mut seen = HashSet::with_capacity(MAX_COMPLETION_ARTIFACTS);
        if let Some(evidence) = raw_evidence {
            let validated = match workspace.as_ref() {
                Some(workspace) => workspace
                    .open_artifact_for_run(&evidence.reference, self.core.run_id())
                    .await
                    .map(|(normalized, _file)| normalized),
                None => Err(AgentError::InvalidRequest(
                    "raw assistant evidence requires a trusted artifact workspace".into(),
                )),
            };
            match validated {
                Ok(reference) => {
                    seen.insert(reference.clone());
                    merged_artifacts.push(reference);
                }
                Err(error) => {
                    // This is runtime-owned best-effort evidence: losing it
                    // must be visible but must not make `/done` permanently
                    // uncallable. Model-proposed refs below remain strict.
                    self.state.last_assistant_artifact = None;
                    let _ = self
                        .core
                        .emit_warning(format!(
                            "raw assistant evidence from turn {} was unavailable at completion: {error}",
                            evidence.turn_id
                        ))
                        .await;
                }
            }
        }

        let mut dropped_for_cap = 0usize;
        for artifact in artifacts {
            let Some(workspace) = workspace.as_ref() else {
                return Err(AgentError::InvalidRequest(
                    "completion artifacts require a trusted artifact workspace".into(),
                ));
            };
            let (normalized, _file) = workspace
                .open_artifact_for_run(&artifact, self.core.run_id())
                .await?;
            if !seen.insert(normalized.clone()) {
                continue;
            }
            if merged_artifacts.len() < MAX_COMPLETION_ARTIFACTS {
                merged_artifacts.push(normalized);
            } else {
                dropped_for_cap = dropped_for_cap.saturating_add(1);
            }
        }
        if dropped_for_cap > 0 {
            let _ = self
                .core
                .emit_warning(format!(
                    "completion artifact cap kept raw evidence first and omitted {dropped_for_cap} proposal ref(s)"
                ))
                .await;
        }
        // The exact final-output body is the completion summary itself in
        // this prototype: retain its digest so the outcome stays
        // byte-for-byte verifiable, with a deterministic ref naming the
        // task's completion record.
        let final_output_digest = Some(crate::task::sha256_hex(summary.as_bytes()));
        let final_output_ref = self
            .state
            .tasks
            .active()
            .map(|task_id| format!("task:{task_id}:completion"));
        let (verification_status, verification_refs) = {
            let state = if let Some(turn) = self.state.turn.as_ref() {
                Some(&turn.execution)
            } else {
                self.state.tasks.get(active_task).map(|task| &task.resume)
            };
            match state {
                Some(state) => crate::task::completion_from_execution(state)?,
                None => (
                    crate::task::CompletionVerificationStatus::Unverified,
                    Vec::new(),
                ),
            }
        };
        let Some((txn, record)) = self.state.tasks.prepare_complete(
            summary.clone(),
            final_output_ref,
            final_output_digest,
            merged_artifacts,
            verification_status,
            verification_refs,
        ) else {
            return Err(AgentError::InvalidRequest(
                "no active task to complete".into(),
            ));
        };
        let task_id = record.task_id;
        let anchor_revision = record.anchor_revision;
        let event_summary = record.summary.clone();
        self.bump_generation()?;
        let report = self
            .services
            .complete_current_task(task_id, summary)
            .await
            .map_err(|error| self.context_transition_failed(error))?;
        self.state.tasks.commit(txn);
        self.state.task_id = None;
        self.state.last_assistant_artifact = None;
        self.state.focus_revision = next_focus_revision;
        let transition = self
            .publish_context_transition(
                RuntimeEvent::TaskCompleted {
                    task_id,
                    anchor_revision,
                    summary: event_summary,
                },
                ContextMaintenanceTrigger::TaskCompleted,
                report,
            )
            .await;
        if transition.is_ok() {
            self.compact_after_completion().await;
            self.run_storage_gc_at_boundary().await;
        }
        transition
    }

    /// When the model stops calling tools, the turn's tool observations
    /// become the long-term record, then the final assistant message. Each
    /// observation is tagged with the tool scope that produced it. Context
    /// directives were already executed at operation-commit time (see
    /// `execute_directive`), so finalization only persists observations.
    ///
    /// Finalization is a commit: every mandatory state write (observation
    /// ingest, the maintenance passes, GC and their journal events) must
    /// succeed before the turn is `Committed` and `TurnCompleted` is
    /// emitted. On the first failure the commit aborts — later writes would
    /// build on a state that is already inconsistent — and the runtime
    /// journals `TurnCommitFailed` (naming the phase) plus
    /// `RecoveryRequired` instead of pretending the turn completed.
    pub(super) async fn finalize_turn(&mut self, content: String) {
        let assistant_evidence_identity = self
            .state
            .task_id
            .zip(self.state.turn.as_ref().map(|turn| turn.turn_id));
        let mut pending_assistant_evidence = None;
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::ModelFinished;
        }
        let mut ingested = false;
        if let Some(turn) = self.state.turn.as_mut() {
            for step in &turn.turn_frame.steps {
                let TurnFrameStep::ToolResult {
                    output,
                    scope_id,
                    disposition,
                } = step
                else {
                    continue;
                };
                // Transient results (context search/inspect/fetch) stay out
                // of the long-term context: reading evidence must not
                // duplicate it under a new observation id. The engine
                // already stamped access on the read itself.
                if *disposition != ToolResultDisposition::PersistObservation {
                    continue;
                }
                if let Err(error) = self
                    .services
                    .context_ingest(ContextIngress::ToolObservation {
                        output: output.clone(),
                        scope_id: *scope_id,
                    })
                    .await
                {
                    return self
                        .commit_failed(TurnCommitPhase::ToolObservationIngest, error)
                        .await;
                }
                ingested = true;
            }
        }
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::Committing;
        }
        if ingested {
            let report = match self
                .services
                .context_maintain(ContextMaintenanceTrigger::AfterTool)
                .await
            {
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
        }
        // Raw-evidence retention: the exact final assistant response is
        // persisted in full *before* the bounded ContextItem is built, so
        // the raw output survives ContextItem truncation and stays
        // recoverable even when the engine's copy was capped. The artifact
        // name embeds a fresh uuid, so sibling responses never overwrite
        // each other. A failure here aborts the commit exactly like any
        // other mandatory state write.
        if let Some(workspace) = self.services.artifact_workspace() {
            use tokio::io::AsyncWriteExt;
            let mut draft = match workspace
                .create_artifact(self.core.run_id(), "assistant-response", "txt")
                .await
            {
                Ok(draft) => draft,
                Err(error) => {
                    return self
                        .commit_failed(TurnCommitPhase::AssistantMessageArtifact, error)
                        .await;
                }
            };
            if let Err(error) = draft.write_all(content.as_bytes()).await {
                return self
                    .commit_failed(
                        TurnCommitPhase::AssistantMessageArtifact,
                        AgentError::Io(format!("write assistant-response artifact: {error}")),
                    )
                    .await;
            }
            let artifact_ref = match workspace.seal_artifact(draft).await {
                Ok(reference) => reference,
                Err(error) => {
                    return self
                        .commit_failed(TurnCommitPhase::AssistantMessageArtifact, error)
                        .await;
                }
            };
            if let Some((task_id, turn_id)) = assistant_evidence_identity {
                pending_assistant_evidence = Some(AssistantArtifactEvidence {
                    task_id,
                    turn_id,
                    reference: artifact_ref,
                });
            }
        }
        if let Err(error) = self
            .services
            .context_ingest(ContextIngress::AssistantMessage {
                content: content.clone(),
            })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AssistantMessageIngest, error)
                .await;
        }
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::AssistantMessage { content })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AssistantMessageEvent, error)
                .await;
        }
        let report = match self
            .services
            .context_maintain(ContextMaintenanceTrigger::AfterModel)
            .await
        {
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
        // Turn boundary: the full GC pass compacts what the per-event
        // residency machine demoted. Eviction is reversible, and the report
        // explains every eviction and reactivation. Push TaskProgress
        // checked paths first so covered file bodies stay Warm/Stored.
        self.push_gc_projections(false).await;
        let report = match self.services.context_gc().await {
            Ok(report) => report,
            Err(error) => {
                return self.commit_failed(TurnCommitPhase::Gc, error).await;
            }
        };
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ContextGc { report })
            .await
        {
            return self.commit_failed(TurnCommitPhase::GcEvent, error).await;
        }
        // 输入记录的 Consumed/Archived 必须在 TurnCompleted 屏障之前入账，
        // 这样 flush 覆盖它们，且 TurnCompleted 仍是屏障前最后一条事件。
        self.emit_input_consumed().await;
        if let Some(applied) = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.applied_input.clone())
        {
            self.emit_input_archived(applied).await;
        }
        // The durability barrier: `emit_event_durable` appends TurnCompleted
        // and then flushes the event journal, so every mandatory state write
        // before it (tool observations, assistant message, maintains, GC)
        // has left the process before the turn is Committed — the channel
        // is FIFO, so the flush covers everything appended before it. A
        // failed barrier means the trace has a gap: the turn is not
        // Committed, and TurnCompleted is never broadcast.
        if let Err(error) = self
            .core
            .emit_event_durable(RuntimeEvent::TurnCompleted)
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::TurnCompletedEvent, error)
                .await;
        }
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::Committed;
        }
        if let Some(turn) = self.state.turn.as_ref()
            && let Some(task_id) = self.state.task_id
        {
            self.state
                .tasks
                .install_resume(task_id, turn.execution.clone());
        }
        // Publish the evidence locator only after the same durable barrier as
        // the turn. A failed commit must not leave a later `/done` pointing
        // at output from a turn the runtime never declared committed.
        self.state.last_assistant_artifact = pending_assistant_evidence;
        // A `task.complete` proposal must run at the safe point — after the
        // turn is durably committed and no operation is in flight — through
        // the same CTX-10 transaction as `/done`. A completion failure here
        // is surfaced, never allowed to undo the committed turn.
        let pending_completion = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.pending_completion.clone());
        self.state.turn = None;
        if let Some(proposal) = pending_completion {
            self.process_pending_completion(proposal).await;
        }
    }

    /// Run a deferred structured completion proposal after the turn
    /// committed. This is the model-side `task.complete` path: the proposal
    /// becomes the active task's typed CompletionRecord. No active task
    /// (suspended/completed meanwhile) drops the proposal with a warning —
    /// it never fails the already-committed turn.
    pub(super) async fn process_pending_completion(&mut self, proposal: CompletionProposal) {
        if self.state.tasks.active().is_none() {
            let _ = self
                .core
                .emit_warning("completion proposal dropped: no active task".to_string())
                .await;
            return;
        }
        let Some(next_focus_revision) = self.next_focus_revision().ok() else {
            return;
        };
        if let Err(error) = self
            .commit_completion(proposal.summary, proposal.artifacts, next_focus_revision)
            .await
        {
            let _ = self
                .core
                .emit_warning(format!("completion proposal failed: {error}"))
                .await;
        }
    }

    /// Abort the turn commit: journal the failed phase and the recovery
    /// requirement, then drop the turn frame. No further mandatory writes
    /// happen after a failure — they would build on a state that is already
    /// inconsistent.
    pub(super) async fn commit_failed(&mut self, phase: TurnCommitPhase, error: AgentError) {
        // A failed mandatory turn write means the runtime can no longer
        // prove that context, the event trace and the caller-visible turn
        // outcome describe the same state. Publishing RecoveryRequired is
        // not itself a fence: persist the poisoned state so every later
        // mutation is rejected until a known-good full restore succeeds.
        self.state.recovery_required = true;
        let _ = self
            .core
            .emit_event(RuntimeEvent::TurnCommitFailed {
                phase: phase.as_str().into(),
                message: error.to_string(),
            })
            .await;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        self.state.turn = None;
    }

    /// Abort a model-round preparation on an engine/journal fault: same
    /// doctrine as `commit_failed`, one step earlier in the round. The
    /// failed phase is journaled durably, mutation is fenced until a
    /// known-good restore, the applied input is settled so it cannot dangle
    /// at Applied, and the turn frame is dropped.
    pub(super) async fn fail_round_preparation(&mut self, phase: &'static str, error: AgentError) {
        self.state.recovery_required = true;
        let _ = self
            .core
            .emit_event(RuntimeEvent::TurnCommitFailed {
                phase: phase.into(),
                message: error.to_string(),
            })
            .await;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        self.settle_aborted_turn().await;
    }

    /// Close the tool scopes this turn still owes a close for. Called
    /// before each model round (drains the exactly-once queue) and on
    /// cancellation, which additionally closes the in-flight operation's
    /// scope — its result will never land to enqueue itself.
    pub(super) async fn close_tool_frames(&mut self) -> AgentResult<()> {
        self.close_tool_scopes(false).await
    }

    /// Shared body: drain `pending_scope_closes`, optionally adding the
    /// in-flight op's scope. Already-closed ids are engine-side no-ops.
    pub(super) async fn close_tool_scopes(&mut self, include_in_flight: bool) -> AgentResult<()> {
        let mut scope_ids: Vec<ScopeId> = Vec::new();
        if let Some(turn) = self.state.turn.as_mut() {
            while let Some(scope_id) = turn.pending_scope_closes.pop_front() {
                scope_ids.push(scope_id);
            }
            if include_in_flight
                && let Some(op) = turn.op.as_ref()
                && let Some(id) = op.scope_id
            {
                scope_ids.push(id);
            }
        }
        scope_ids.sort_unstable_by_key(|scope_id| scope_id.to_string());
        scope_ids.dedup();
        let total_deadline = tokio::time::Instant::now() + TOOL_SCOPE_CLOSE_TOTAL_TIMEOUT;
        for scope_id in scope_ids {
            let remaining = total_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AgentError::RecoveryRequired(
                    "tool-scope cleanup exceeded its total deadline".into(),
                ));
            }
            let timeout = remaining.min(TOOL_SCOPE_CLOSE_TIMEOUT);
            match tokio::time::timeout(timeout, self.services.context_close_scope(scope_id)).await {
                Err(_) => {
                    return Err(AgentError::RecoveryRequired(format!(
                        "closing tool scope {scope_id} exceeded the {timeout:?} deadline"
                    )));
                }
                Ok(result) => match result {
                    Ok(transitions) => {
                        // The close is an auditable result: publish the
                        // lifecycle transitions it produced (a tool frame's
                        // durable outcomes promoted out of the frame). An empty
                        // transition list is a no-op close — nothing to report.
                        if !transitions.is_empty() {
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::ToolScopeClosed {
                                    scope_id,
                                    transitions,
                                })
                                .await;
                        }
                    }
                    Err(error) => {
                        return Err(AgentError::RecoveryRequired(format!(
                            "closing tool scope {scope_id} failed: {error}"
                        )));
                    }
                },
            }
        }
        Ok(())
    }

    /// Cancel the active turn and durably publish its distinct terminal
    /// state. Cancellation is effective before the barrier (the operation
    /// is fenced and its late completion is stale), but a failed barrier
    /// returns `RecoveryRequired` and poisons ordinary mutation rather than
    /// pretending the cancellation was durably acknowledged.
    pub(super) async fn cancel_turn(
        &mut self,
        reason: TurnCancellationReason,
        operation_id_override: Option<OperationId>,
    ) -> AgentResult<TurnCancelAck> {
        let Some(turn) = self.state.turn.as_ref() else {
            return Ok(TurnCancelAck::NoActiveTurn);
        };
        let cancelled_generation = self.state.generation;
        let operation_id = turn
            .op
            .as_ref()
            .map(|operation| operation.operation_id)
            .or(operation_id_override);
        let cleanup_kind = turn.op.as_ref().map(|operation| operation.kind);
        // Install the Core-owned fence before any await or cleanup. A tool
        // completion racing cancellation can no longer commit an effect
        // while scope closure is blocked.
        let effective_generation = self.bump_generation()?;
        let tool_identity = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.op.as_ref())
            .and_then(|operation| operation.tool_identity.clone());
        if let Some(operation) = self.state.turn.as_ref().and_then(|turn| turn.op.as_ref()) {
            operation.cancel.cancel();
        }
        if let Some(identity) = tool_identity
            && let Err(error) = self.core.cancel_operation(identity)
        {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(AgentError::RecoveryRequired(format!(
                "Core could not install the cancelled operation terminal: {error}"
            )));
        }
        if cleanup_kind == Some(OpKind::Tool) {
            self.state.pending_tool_cleanup = operation_id;
        }
        if let Err(error) = self.close_tool_scopes(true).await {
            self.state.recovery_required = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(error.to_string()),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(error);
        }
        let mut turn = self
            .state
            .turn
            .take()
            .expect("the active turn was inspected immediately above");
        turn.op = None;
        let event = RuntimeEvent::TurnCancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
            reason,
        };
        if let Err(error) = self.core.emit_event_durable(event).await {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            return Err(AgentError::RecoveryRequired(format!(
                "turn {} was cancelled, but its audit barrier failed: {error}",
                turn.turn_id
            )));
        }
        self.publish_interrupt_committed(
            turn.turn_id,
            turn.applied_input.as_ref().and_then(|input| input.input_id),
            reason,
        )
        .await;
        Ok(TurnCancelAck::Cancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
        })
    }

    /// Complete the actor-owned cleanup and durable cancellation event after
    /// Core has atomically installed both the new epoch and (for a tool)
    /// cancellation terminal.
    pub(super) async fn finish_cancelled_turn(
        &mut self,
        reason: TurnCancellationReason,
        operation_id_override: Option<OperationId>,
        cancelled_generation: u64,
        effective_generation: u64,
    ) -> AgentResult<TurnCancelAck> {
        self.state.generation = effective_generation;
        let Some(turn) = self.state.turn.as_ref() else {
            return self
                .operation_control_recovery(
                    "Core installed an operation cancellation after the active turn disappeared"
                        .into(),
                )
                .await;
        };
        let operation_id = turn
            .op
            .as_ref()
            .map(|operation| operation.operation_id)
            .or(operation_id_override);
        let cleanup_kind = turn.op.as_ref().map(|operation| operation.kind);
        if let Some(operation) = turn.op.as_ref() {
            operation.cancel.cancel();
        }
        if cleanup_kind == Some(OpKind::Tool) {
            self.state.pending_tool_cleanup = operation_id;
        }
        if let Err(error) = self.close_tool_scopes(true).await {
            self.state.recovery_required = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(error.to_string()),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(error);
        }
        let mut turn = self
            .state
            .turn
            .take()
            .expect("the active turn was inspected immediately above");
        turn.op = None;
        let event = RuntimeEvent::TurnCancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
            reason,
        };
        if let Err(error) = self.core.emit_event_durable(event).await {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            return Err(AgentError::RecoveryRequired(format!(
                "turn {} was cancelled, but its audit barrier failed: {error}",
                turn.turn_id
            )));
        }
        self.publish_interrupt_committed(
            turn.turn_id,
            turn.applied_input.as_ref().and_then(|input| input.input_id),
            reason,
        )
        .await;
        Ok(TurnCancelAck::Cancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
        })
    }

    /// Cancel exactly the active tool operation, then return Core's durable
    /// post-cancellation truth. The complete identity comparison is the
    /// trusted-boundary canonicalization step: a caller cannot retarget a
    /// current turn by supplying only a matching operation id.
    pub(super) async fn cancel_operation(
        &mut self,
        identity: ToolOperationIdentity,
    ) -> AgentResult<OperationQueryResult> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        if self.state.recovery_required {
            return Err(AgentError::RecoveryRequired(
                "runtime recovery is required before operation cancellation may continue".into(),
            ));
        }
        if let AuthorityRecoveryStatus::RecoveryRequired { reason } = self.core.recovery_status() {
            return self
                .operation_control_recovery(format!(
                    "Core authority recovery is required before operation cancellation: {reason}"
                ))
                .await;
        }

        let active_identity = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.op.as_ref())
            .filter(|operation| operation.kind == OpKind::Tool)
            .and_then(|operation| operation.tool_identity.as_ref())
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "operation cancellation requires a current in-flight tool operation".into(),
                )
            })?;
        if active_identity != &identity {
            return Err(AgentError::InvalidRequest(
                "operation identity does not match the current in-flight tool operation".into(),
            ));
        }

        let cancelled_generation = self.state.generation;
        let disposition = match self
            .core
            .cancel_operation_and_advance(identity.clone(), cancelled_generation)
        {
            Ok(disposition) => disposition,
            Err(AgentError::RecoveryRequired(reason)) => {
                return self
                    .operation_control_recovery(format!(
                        "Core could not durably cancel operation {}: {reason}",
                        identity.operation_id
                    ))
                    .await;
            }
            Err(error) => return Err(error),
        };
        let (effective_generation, result) = match disposition {
            OperationCancelDisposition::AlreadySettled(result) => {
                if matches!(result, OperationQueryResult::ExpiredOrPossiblySeen) {
                    return self
                        .operation_control_recovery(format!(
                            "operation {} is expired or only conservatively known; cancellation cannot infer its state",
                            identity.operation_id
                        ))
                        .await;
                }
                return Ok(result);
            }
            OperationCancelDisposition::Cancelled {
                effective_epoch,
                result,
            } => (effective_epoch, result),
        };
        let acknowledgement = self
            .finish_cancelled_turn(
                TurnCancellationReason::Requested,
                Some(identity.operation_id),
                cancelled_generation,
                effective_generation,
            )
            .await?;
        if !matches!(
            acknowledgement,
            TurnCancelAck::Cancelled {
                operation_id: Some(operation_id),
                ..
            } if operation_id == identity.operation_id
        ) {
            return self
                .operation_control_recovery(format!(
                    "operation {} cancellation did not produce its durable turn acknowledgement",
                    identity.operation_id,
                ))
                .await;
        }

        let cancelled = matches!(
            &result,
            OperationQueryResult::Found { snapshot }
                if snapshot.identity == identity
                    && matches!(
                        snapshot.state,
                        OperationState::Terminal {
                            terminal: OperationTerminal::CancelledBeforeCommit,
                            ..
                        }
                    )
        );
        if cancelled {
            Ok(result)
        } else {
            self.operation_control_recovery(format!(
                "operation {} passed the cancellation barrier without a matching Core terminal",
                identity.operation_id,
            ))
            .await
        }
    }

    pub(super) async fn operation_control_recovery<T>(
        &mut self,
        message: String,
    ) -> AgentResult<T> {
        self.state.recovery_required = true;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        Err(AgentError::RecoveryRequired(format!(
            "{message}; runtime remains fenced"
        )))
    }
}

use super::*;

impl RuntimeActor {
    /// Cross the one-shot startup durability boundary. A failed append or
    /// flush may leave a forensic prefix in the journal, so this actor must
    /// never retry startup or accept work that could later be mistaken for a
    /// committed legacy turn.
    pub(super) async fn start_serving(&mut self) -> AgentResult<()> {
        match self.state.lifecycle {
            ActorLifecycle::NotStarted => match self.core.start().await {
                Ok(()) => {
                    self.state.lifecycle = ActorLifecycle::Serving;
                    Ok(())
                }
                Err(error) => {
                    self.state.lifecycle = ActorLifecycle::StartFailed;
                    self.state.recovery_required = true;
                    Err(AgentError::RecoveryRequired(format!(
                        "runtime startup failed before the serving marker committed: {error}"
                    )))
                }
            },
            ActorLifecycle::Serving => Err(AgentError::InvalidRequest(
                "runtime is already started".into(),
            )),
            ActorLifecycle::StartFailed => Err(AgentError::RecoveryRequired(
                "runtime startup previously failed; this actor cannot enter service".into(),
            )),
        }
    }

    /// Read-only recovery inspection remains available outside service, but
    /// every command that can change runtime, context, task or journal state
    /// crosses this gate first.
    pub(super) fn ensure_serving(&self) -> AgentResult<()> {
        match self.state.lifecycle {
            ActorLifecycle::Serving => Ok(()),
            ActorLifecycle::NotStarted => Err(AgentError::InvalidRequest(
                "runtime must be started before accepting work".into(),
            )),
            ActorLifecycle::StartFailed => Err(AgentError::RecoveryRequired(
                "runtime startup failed; this actor is recovery-fenced".into(),
            )),
        }
    }

    /// A turn is accepted only when the runtime is idle. Serializing every
    /// mutation removes the structural race where focus/pin/task commands
    /// interleaved with an in-flight turn.
    pub(super) fn ensure_idle(&self) -> AgentResult<()> {
        self.ensure_serving()?;
        if self.state.recovery_required {
            Err(AgentError::RecoveryRequired(
                "runtime recovery is required before normal mutation may continue".into(),
            ))
        } else if let Some(operation_id) = self.state.pending_tool_cleanup {
            Err(AgentError::InvalidRequest(format!(
                "agent is finishing explicit cleanup for cancelled tool operation {operation_id}"
            )))
        } else if self
            .state
            .gc_work
            .as_ref()
            .is_some_and(|pending| pending.is_commit_in_flight())
        {
            // EXEC-7 (R2-08): a task-completion or safe-point commit is in
            // flight as a spawned boundary operation. Its prepared task
            // transaction must apply over exactly the task table it was
            // prepared against, so no new mutation is admitted until the
            // commit settles.
            Err(AgentError::InvalidRequest(
                "a task-completion / checkpoint commit is settling; retry when it completes".into(),
            ))
        } else {
            self.ensure_no_active_turn()
        }
    }

    /// Ask the approval gate whether a boundary anchor patch (goal /
    /// constraints / waiver) may proceed. The patch is presented as a
    /// synthetic `task.anchor` tool call so existing approval policies (and
    /// the v2 shadow gate) see a typed, serializable request instead of a
    /// side channel. The gate decides; a deny or a failed check errors out
    /// without touching the task table.
    pub(super) async fn authorize_anchor_patch(&self, patch: &AnchorPatch) -> AgentResult<()> {
        let arguments = serde_json::to_value(patch).map_err(|error| {
            AgentError::Internal(format!("anchor patch serialization: {error}"))
        })?;
        let call = ToolCall {
            id: format!("anchor-patch-{}", RunId::new()),
            name: "task.anchor".into(),
            arguments,
        };
        let spec = agent_contracts::ToolSpec {
            name: "task.anchor".into(),
            description: "Patch the task anchor; goal/constraint fields require approval".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: Vec::new(),
        };
        let verdict = self
            .core
            .authorize(&call, &spec, &CancellationToken::new())
            .await;
        match verdict {
            ApprovalVerdict::Allowed => Ok(()),
            ApprovalVerdict::Denied(message) | ApprovalVerdict::Failed(message) => {
                Err(AgentError::InvalidRequest(format!(
                    "boundary anchor patch denied by approval policy: {message}"
                )))
            }
        }
    }

    pub(super) fn next_focus_revision(&self) -> AgentResult<u64> {
        self.state
            .focus_revision
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("runtime focus revision is exhausted".into()))
    }

    /// Ask trusted Core to advance the process-lifetime commit fence. The
    /// actor remains the sole lifecycle scheduler; Core owns only the
    /// monotonic authority value and rejects stale or forged commits.
    pub(super) fn bump_generation(&mut self) -> AgentResult<u64> {
        match self.core.advance_authority_epoch(self.state.generation) {
            Ok(epoch) => {
                self.state.generation = epoch;
                Ok(epoch)
            }
            Err(error) => {
                self.state.recovery_required = true;
                Err(error)
            }
        }
    }

    pub(super) fn issue_surface_revision(&mut self) -> AgentResult<u64> {
        let revision = self
            .state
            .last_surface_revision
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("round surface revision is exhausted".into()))?;
        self.state.last_surface_revision = revision;
        Ok(revision)
    }

    pub(super) fn ensure_no_active_turn(&self) -> AgentResult<()> {
        if self.state.turn.is_some() {
            Err(AgentError::InvalidRequest(
                "agent is busy: a turn is already running".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Shared focus transition for `SetFocus` and `StartWork` (P1): create
    /// or resume the task for `goal`, switch the engine's focus, and commit
    /// the TaskManager transition only after the engine succeeded, so the two
    /// planes can never diverge. An oversized goal or a saturated catalog
    /// fails closed here, before any engine or event mutation.
    pub(super) async fn apply_focus(&mut self, goal: String) -> AgentResult<TaskId> {
        let next_focus_revision = self.next_focus_revision()?;
        let (txn, task_id) = self.state.tasks.prepare_create(&goal)?;
        let event_goal = goal.clone();
        self.bump_generation()?;
        match self.services.set_focus(task_id, goal).await {
            Ok(report) => {
                self.state.tasks.commit(txn);
                self.state.task_id = Some(task_id);
                self.state.last_assistant_artifact = None;
                self.state
                    .task_requirement_high_water
                    .entry(task_id)
                    .or_insert(0);
                self.state.focus_revision = next_focus_revision;
                self.publish_context_transition(
                    RuntimeEvent::FocusChanged {
                        task_id,
                        goal: event_goal,
                    },
                    ContextMaintenanceTrigger::FocusChanged,
                    report,
                )
                .await?;
                Ok(task_id)
            }
            Err(error) => Err(self.context_transition_failed(error)),
        }
    }

    /// Shared whole-set tool-requirement replace behind the CAS boundary,
    /// used by the `ReplaceTaskToolRequirements` command and by `StartWork`'s
    /// long-task checklist attach. The caller owns the idle check.
    pub(super) async fn set_task_tool_requirements(
        &mut self,
        task_id: TaskId,
        base_revision: u64,
        entries: Vec<ToolSurfaceRequirement>,
    ) -> AgentResult<u64> {
        let entries = crate::task::normalize_tool_requirements(entries)?;
        let (txn, revision) = self.state.tasks.prepare_replace_tool_requirements(
            task_id,
            base_revision,
            entries.clone(),
        )?;
        let changed = revision != base_revision;
        if changed {
            self.bump_generation()?;
            self.core
                .emit_event(RuntimeEvent::TaskToolRequirementsChanged {
                    task_id,
                    revision,
                    requirements: entries,
                })
                .await?;
            self.state.tasks.commit(txn);
            self.state
                .task_requirement_high_water
                .insert(task_id, revision);
        } else {
            self.state.tasks.commit(txn);
        }
        Ok(revision)
    }

    pub(super) fn context_transition_failed(&mut self, error: AgentError) -> AgentError {
        if matches!(&error, AgentError::RecoveryRequired(_)) {
            self.state.recovery_required = true;
            let core = self.core.clone();
            tokio::spawn(async move {
                let _ = core.emit_event(RuntimeEvent::RecoveryRequired).await;
            });
        }
        error
    }

    /// Publish the audit/UI events for an already committed context/task
    /// transition. Event persistence may still fail, but it can no longer
    /// leave the context plane ahead of the task authority plane.
    pub(super) async fn publish_context_transition(
        &mut self,
        event: RuntimeEvent,
        trigger: ContextMaintenanceTrigger,
        report: ContextMaintenanceReport,
    ) -> AgentResult<()> {
        if let Err(error) = self.core.emit_event(event).await {
            return Err(self.audit_gap_after_commit(error).await);
        }
        if let Err(error) = self.emit_context_maintained(trigger, report).await {
            return Err(self.audit_gap_after_commit(error).await);
        }
        Ok(())
    }

    pub(super) async fn emit_context_maintained(
        &self,
        trigger: ContextMaintenanceTrigger,
        report: ContextMaintenanceReport,
    ) -> AgentResult<()> {
        for event in context_maintenance_events(trigger, report) {
            self.core.emit_event(event).await?;
        }
        Ok(())
    }

    pub(super) async fn audit_gap_after_commit(&mut self, error: AgentError) -> AgentError {
        self.state.recovery_required = true;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        AgentError::RecoveryRequired(format!(
            "context/task transition committed, but its audit event failed ({error})"
        ))
    }

    /// GC/Storage GC 前把当前活跃任务的 anchor 根声明投影给引擎。
    /// ResidentRequired/PromptRequired 的声明保护（或召回）工作集条目，
    /// StorageRequired 的声明保护 store 留存。任务权威留在 TaskManager，
    /// 这里只导出有界投影；推送失败不阻塞 GC——引擎仍按已推送的根集
    /// 运行（失败以 Error 事件暴露，绝不静默）。`force` 时即使投影为空
    /// 也推送（完成边界用它清掉旧声明，让完成任务的记录不再被保护）；
    /// 否则空投影跳过，不打扰既有的 directive 语义。
    pub(super) async fn push_anchor_roots_for_gc(&self, force: bool) {
        let roots = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| crate::task::anchor_root_claims(&task.anchor))
            .unwrap_or_default();
        if roots.is_empty() && !force {
            return;
        }
        if let Err(error) = self
            .services
            .context_ingest(ContextIngress::ContextDirective {
                action: agent_contracts::ContextAction::AnchorRoots { roots },
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!("failed to push anchor roots before GC: {error}"),
                })
                .await;
        }
    }

    pub(super) async fn push_checked_files_for_gc(&self) {
        let files = self.projected_checked_files();
        if let Err(error) = self
            .services
            .context_ingest(ContextIngress::ContextDirective {
                action: agent_contracts::ContextAction::CheckedFiles { files },
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!("failed to push checked files before GC: {error}"),
                })
                .await;
        }
    }

    fn projected_checked_files(&self) -> Vec<String> {
        if !self.services.project_task_progress() {
            return Vec::new();
        }
        let Some(task) = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
        else {
            return Vec::new();
        };
        let view = match self.state.turn.as_ref() {
            Some(turn) => turn.execution.view(),
            None => task.resume.view(),
        };
        view.checked_files
    }

    pub(super) async fn push_gc_projections(&self, force_anchor: bool) {
        self.push_anchor_roots_for_gc(force_anchor).await;
        self.push_checked_files_for_gc().await;
    }

    /// EXEC-7 (R2-08): the post-completion boundary's resume half. The
    /// completion itself is already committed; a GC failure is surfaced as
    /// an `Error` event and never rolls the outcome back. The full pass and
    /// the storage boundary ran in the spawned boundary operation; only
    /// their audit events are emitted here, in the same order the inline
    /// version used.
    pub(super) async fn finish_completion_boundary(
        &mut self,
        gc: AgentResult<agent_contracts::ContextGcReport>,
        storage: AgentResult<agent_contracts::StorageGcReport>,
    ) {
        // CTX-8 接线 (R3-08): observe the boundary pass for the status
        // snapshot. A backpressured pass throttles NEW body production; a
        // clean pass lifts the throttle. The observation rides the typed
        // status snapshot — the throttle itself lives in the engine's
        // pending/owned items, never in a second GC authority.
        self.state.store_backpressure = match (&gc, &storage) {
            (Ok(gc_report), _) => Some(crate::work::StoreBackpressure {
                active: gc_report.externalize_backpressure,
                externalize_deferred: gc_report.externalize_deferred,
                store_io_failures: gc_report.store_io_failures,
            }),
            (Err(_), _) => None,
        };
        match gc {
            Ok(report) => {
                if let Err(error) = self
                    .core
                    .emit_event(RuntimeEvent::ContextGc { report })
                    .await
                {
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!("post-completion GC failed: {error}"),
                    })
                    .await;
            }
        }
        match storage {
            Ok(report) => {
                if let Err(error) = self
                    .core
                    .emit_event(RuntimeEvent::StorageGc { report })
                    .await
                {
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!("storage GC at task completion failed: {error}"),
                    })
                    .await;
            }
        }
    }

    /// 把用户正文写入证据平面一次。没有 artifact workspace 时返回空引用，
    /// 事件仍然只带有界预览。
    pub(super) async fn persist_user_input_body(
        &self,
        content: &str,
    ) -> AgentResult<(Option<String>, Option<String>)> {
        let Some(workspace) = self.services.artifact_workspace() else {
            return Ok((None, None));
        };
        let reference = workspace
            .write_artifact(
                self.core.run_id(),
                USER_INPUT_ARTIFACT_OWNER,
                "txt",
                content.as_bytes(),
            )
            .await?;
        let digest = ArtifactLocator::parse(&reference)?
            .digest()
            .map(|digest| digest.to_string());
        Ok((Some(reference), digest))
    }

    pub(super) async fn emit_user_input(&self, input: RuntimeInputEnvelope) -> AgentResult<()> {
        if let Err(reason) = input.validate() {
            return Err(AgentError::InvalidRequest(reason));
        }
        self.core
            .emit_event(RuntimeEvent::UserMessageAccepted { input })
            .await
    }

    /// 清理中的 UserMessage fail closed，留下 Rejected。RecoveryRequired 是栅栏。
    pub(super) async fn record_rejected_user_dialogue(&self, content: &str) -> AgentResult<()> {
        let input = RuntimeInputEnvelope::user_dialogue(
            content.to_owned(),
            Some(RuntimeInputId::new()),
            self.state.task_id,
            None,
            None,
            None,
        )
        .with_lifecycle(InputLifecycle::Rejected);
        self.emit_user_input(input).await
    }

    pub(super) fn cancellation_preview(reason: TurnCancellationReason) -> &'static str {
        match reason {
            TurnCancellationReason::Requested => "cancel turn",
            TurnCancellationReason::OperationCancelled => "operation cancelled",
            TurnCancellationReason::Shutdown => "shutdown",
        }
    }

    pub(super) async fn publish_interrupt_committed(
        &self,
        turn_id: TurnId,
        causal_parent: Option<RuntimeInputId>,
        reason: TurnCancellationReason,
    ) {
        let preview = Self::cancellation_preview(reason);
        let input = RuntimeInputEnvelope {
            preview: bounded_preview(preview, USER_INPUT_PREVIEW_CHARS),
            input_id: Some(RuntimeInputId::new()),
            task_id: self.state.task_id,
            turn_id: Some(turn_id),
            causal_parent,
            source: InputSource::User,
            authority: InputAuthority::UserSteering,
            kind: InputKind::CancelTurn,
            lifecycle: InputLifecycle::InterruptCommitted,
            body_ref: None,
            digest: None,
            bytes: preview.len() as u64,
            proposal: StatePatchProposal::None,
        };
        let _ = self.emit_user_input(input).await;
    }

    pub(super) async fn emit_input_consumed(&mut self) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        if turn.input_consumed {
            return;
        }
        let Some(applied) = turn.applied_input.clone() else {
            return;
        };
        turn.input_consumed = true;
        let _ = self
            .emit_user_input(applied.with_lifecycle(InputLifecycle::Consumed))
            .await;
    }

    pub(super) async fn emit_input_archived(&self, applied: RuntimeInputEnvelope) {
        let _ = self
            .emit_user_input(applied.with_lifecycle(InputLifecycle::Archived))
            .await;
    }

    /// Settle a turn that ends without its commit barrier (a refused round,
    /// an exhausted round budget, a failed provider call): the applied user
    /// input must not dangle at Applied forever. The committed interruption
    /// is the input's terminal audit record, then the turn frame is dropped.
    pub(super) async fn settle_aborted_turn(&mut self) {
        if let Err(error) = self.settle_action_batch().await {
            self.require_effect_recovery(format!(
                "action-batch audit failed while aborting the turn: {error}"
            ))
            .await;
        }
        if let Some(turn) = self.state.turn.take()
            && !turn.input_consumed
            && let Some(applied) = turn.applied_input
        {
            let _ = self
                .emit_user_input(applied.with_lifecycle(InputLifecycle::InterruptCommitted))
                .await;
        }
    }

    /// 周转中最多排队 `USER_INPUT_QUEUE_CAP` 条。槽满则 Rejected。
    pub(super) async fn queue_user_dialogue(&mut self, content: String) -> AgentResult<()> {
        // 与 start_turn 同一入口策略：超限正文在持久化/入账前拒绝。
        if content.len() > USER_INPUT_MAX_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "user input is {} bytes, above the {USER_INPUT_MAX_BYTES} byte cap",
                content.len()
            )));
        }
        if self.state.pending_user_input.is_some() {
            let _ = self.record_rejected_user_dialogue(&content).await;
            return Err(AgentError::InvalidRequest(format!(
                "agent is busy: a turn is already running and {USER_INPUT_QUEUE_CAP} user message is already queued"
            )));
        }
        let input_id = RuntimeInputId::new();
        let (body_ref, digest) = self.persist_user_input_body(&content).await?;
        let mut input = RuntimeInputEnvelope::user_dialogue(
            content.clone(),
            Some(input_id),
            self.state.task_id,
            None,
            body_ref,
            digest,
        )
        .with_lifecycle(InputLifecycle::Queued);
        input.causal_parent = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.applied_input.as_ref())
            .and_then(|applied| applied.input_id);
        self.emit_user_input(input.clone()).await?;
        self.state.pending_user_input = Some(QueuedUserDialogue { content, input });
        Ok(())
    }

    pub(super) async fn drain_queued_user_input(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        if self.state.recovery_required
            || self.state.turn.is_some()
            || self.state.pending_tool_cleanup.is_some()
        {
            return;
        }
        let Some(queued) = self.state.pending_user_input.take() else {
            return;
        };
        if let Err(error) = self
            .begin_applied_turn(queued.content, queued.input, op_tx)
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!("queued user input failed to start: {error}"),
                })
                .await;
        }
    }
}

use super::*;

impl RuntimeActor {
    /// A turn is accepted only when the runtime is idle. Serializing every
    /// mutation removes the structural race where focus/pin/task commands
    /// interleaved with an in-flight turn.
    pub(super) fn ensure_idle(&self) -> AgentResult<()> {
        if self.state.recovery_required {
            Err(AgentError::RecoveryRequired(
                "runtime recovery is required before normal mutation may continue".into(),
            ))
        } else if let Some(operation_id) = self.state.pending_tool_cleanup {
            Err(AgentError::InvalidRequest(format!(
                "agent is finishing explicit cleanup for cancelled tool operation {operation_id}"
            )))
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

    /// One full GC pass after a task completed, so the finished task's
    /// records leave the resident heap and stay recallable from the
    /// reversible buffer / context store. The completion itself is already
    /// committed; a GC failure is surfaced as an `Error` event and never
    /// rolls the outcome back.
    pub(super) async fn compact_after_completion(&mut self) {
        // 完成边界前的根声明投影：完成任务后 active 通常已切换/清空，
        // 强制推送当前（或空）根集，声明不再保护已完成任务的工作集。
        self.push_gc_projections(true).await;
        match self.services.context_gc().await {
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
    }

    /// Task completion is an explicit runtime boundary for Storage GC: the
    /// completed task's records are storage roots until this point, after
    /// which the only live references are the completion outcome and its
    /// evidence. Run one conservative Storage GC pass here — never on the
    /// per-model hot path — and publish the report so every permanent
    /// deletion is observable and auditable. A failure is surfaced as an
    /// Error event, never allowed to undo the completed task.
    pub(super) async fn run_storage_gc_at_boundary(&mut self) {
        // 完成边界前推送根声明投影：StorageRequired 的声明会让 storage GC
        // 保留其指向的 store 条目（已完成任务的证据留存由声明决定）。
        self.push_gc_projections(true).await;
        match self.services.context_storage_gc().await {
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

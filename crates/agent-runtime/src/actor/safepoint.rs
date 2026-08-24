//! Safe-point resume commits and background checkpoint writes.
//!
//! At a fully settled tool batch (terminal settlement for every member,
//! no operation in flight) accrued checkpoint debt installs the bounded
//! `ExecutionState` into the task resume and schedules exactly one atomic
//! checkpoint write. Read-only exploration accrues nothing. A failed
//! background write keeps the debt visible and retryable; nothing may
//! claim safe resumability until a `CheckpointDurable` event lands.

use super::*;
use crate::checkpoint::{CheckpointDebtReason, CheckpointStore, StoredCheckpoint};

/// Normalize one background checkpoint join into a single result type.
async fn join_checkpoint_write(
    handle: tokio::task::JoinHandle<AgentResult<(u64, StoredCheckpoint)>>,
) -> AgentResult<(u64, StoredCheckpoint)> {
    match handle.await {
        Ok(result) => result,
        Err(join_error) => Err(AgentError::InvalidRequest(format!(
            "checkpoint write task failed: {join_error}"
        ))),
    }
}

impl RuntimeActor {
    /// Record one coalesced reason the next settled batch owes a durable
    /// checkpoint. Idempotent per reason.
    pub(super) fn accrue_checkpoint_debt(&mut self, reason: CheckpointDebtReason) {
        if !self.state.checkpoint_debt.contains(&reason) {
            self.state.checkpoint_debt.push(reason);
        }
    }

    /// Assemble the runtime checkpoint from every plane this process can
    /// see: actor state, engine snapshot, authority marker and the live
    /// host capability surface. The registry handle is a read-only
    /// snapshot source injected at spawn; the actor stays the sole
    /// lifecycle orchestrator and the host performs a mechanical merge.
    pub(super) async fn capture_checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        let context = self.core.checkpoint().await?;
        let authority = self.core.authority_checkpoint_marker()?;
        Ok(RuntimeCheckpoint {
            version: crate::checkpoint::RUNTIME_CHECKPOINT_VERSION,
            run_metadata: crate::checkpoint::RunMetadata {
                run_id: self.core.run_id(),
                created_at_ms: now_ms(),
            },
            tasks: crate::checkpoint::TaskManagerSnapshot::from_manager(&self.state.tasks),
            current_task_id: self.state.task_id,
            focus_revision: self.state.focus_revision,
            last_surface_revision: self.state.last_surface_revision,
            context,
            capabilities: self.services.capability_snapshot(),
            authority,
        })
    }

    fn checkpoint_store(&self) -> Option<CheckpointStore> {
        self.services
            .artifact_workspace()
            .map(|workspace| CheckpointStore::new(workspace.state_dir().join("checkpoints")))
    }

    fn checkpoint_store_missing_error() -> AgentError {
        AgentError::InvalidRequest("no checkpoint store configured".into())
    }

    /// Publish the outcome of a background write that already finished.
    /// A success advances the durable watermark and publishes
    /// `CheckpointDurable` with the acknowledged revision; a failure
    /// re-arms the debt and returns an error so barrier callers can
    /// fail closed.
    async fn settle_finished_checkpoint_write(&mut self) -> AgentResult<()> {
        let finished = matches!(
            self.state.checkpoint_write.as_ref(),
            Some(handle) if handle.is_finished()
        );
        if !finished {
            return Ok(());
        }
        let handle = self
            .state
            .checkpoint_write
            .take()
            .expect("a finished handle is present");
        match join_checkpoint_write(handle).await {
            Ok((revision, stored)) => {
                self.state.checkpoint_write_failed = false;
                self.state.durable_revision =
                    Some(self.state.durable_revision.unwrap_or(0).max(revision));
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision,
                        checksum: stored.checksum,
                    })
                    .await;
                Ok(())
            }
            Err(error) => {
                // The debt reasons were already cleared when the write was
                // scheduled; re-accrue so the next safe point retries.
                self.state.checkpoint_write_failed = true;
                self.accrue_checkpoint_debt(CheckpointDebtReason::TaskAnchorChanged);
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointWriteFailed {
                        reason: bounded_preview(
                            &error.to_string(),
                            agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS,
                        ),
                    })
                    .await;
                Err(error)
            }
        }
    }

    /// LONG-TASK SAFE POINT: the whole requested batch has terminal
    /// settlement and nothing is in flight. Debt coalesces into one
    /// candidate snapshot; several mutations in one batch produce one
    /// resume install and one write. The installed revision becomes the
    /// required durability watermark for continuation.
    pub(super) async fn safe_point_resume_commit(&mut self) {
        let _ = self.settle_finished_checkpoint_write().await;
        if self.state.checkpoint_debt.is_empty() || self.state.checkpoint_write.is_some() {
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
        if let Some(turn) = self.state.turn.as_ref() {
            self.state
                .tasks
                .install_resume(task_id, turn.execution.clone());
        }
        self.state.resume_state_revision = Some(anchor_revision);
        self.state.required_durable_revision = Some(
            self.state
                .required_durable_revision
                .unwrap_or(0)
                .max(anchor_revision),
        );
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
        self.schedule_checkpoint_write(anchor_revision).await;
    }

    /// The active task's anchor revision, or zero when no task is active.
    fn current_anchor_revision(&self) -> u64 {
        self.state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.anchor.revision)
            .unwrap_or_default()
    }

    /// Capture the current planes, serialize them, and hand one atomic
    /// write to the background. Debt clears only when a write is actually
    /// in flight or durability is impossible-by-configuration (surfaced,
    /// never silent).
    async fn schedule_checkpoint_write(&mut self, revision: u64) {
        let capture = self.capture_checkpoint().await;
        let snapshot = match capture {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.state.checkpoint_write_failed = true;
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointWriteFailed {
                        reason: bounded_preview(
                            &error.to_string(),
                            agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS,
                        ),
                    })
                    .await;
                return;
            }
        };
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.state.checkpoint_write_failed = true;
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointWriteFailed {
                        reason: bounded_preview(
                            &format!("checkpoint serialization failed: {error}"),
                            agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS,
                        ),
                    })
                    .await;
                return;
            }
        };
        let Some(store) = self.checkpoint_store() else {
            self.state.checkpoint_debt.clear();
            self.state.checkpoint_write_failed = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::CheckpointWriteFailed {
                    reason: Self::checkpoint_store_missing_error().to_string(),
                })
                .await;
            return;
        };
        self.state.checkpoint_debt.clear();
        self.state.checkpoint_write = Some(tokio::spawn(async move {
            store
                .write_atomic(&bytes)
                .await
                .map(|stored| (revision, stored))
        }));
    }

    /// Barrier wait: explicit pause/suspend/completion/shutdown paths call
    /// this so they never report an outcome whose resume checkpoint is
    /// still in flight. A durable ack advances the watermark and publishes
    /// `CheckpointDurable` with its revision; a failed write surfaces here
    /// as `CheckpointWriteFailed` with debt re-armed and an error return,
    /// so callers can refuse to claim resumability.
    pub(super) async fn await_pending_checkpoint(&mut self) -> AgentResult<()> {
        let Some(handle) = self.state.checkpoint_write.take() else {
            return self.settle_finished_checkpoint_write().await;
        };
        match join_checkpoint_write(handle).await {
            Ok((revision, stored)) => {
                self.state.checkpoint_write_failed = false;
                self.state.durable_revision =
                    Some(self.state.durable_revision.unwrap_or(0).max(revision));
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision,
                        checksum: stored.checksum,
                    })
                    .await;
                Ok(())
            }
            Err(error) => {
                self.state.checkpoint_write_failed = true;
                self.accrue_checkpoint_debt(CheckpointDebtReason::TaskAnchorChanged);
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointWriteFailed {
                        reason: bounded_preview(
                            &error.to_string(),
                            agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS,
                        ),
                    })
                    .await;
                Err(error)
            }
        }
    }

    /// LONGTASK-04: continuation across a segment is allowed only when the
    /// required durability watermark has actually landed. A failed or
    /// missing safe-point write fences `continue_active_task` before any
    /// model request, and stays fenced until a retry succeeds.
    pub(super) async fn continuation_durability_gate(&mut self) -> AgentResult<()> {
        self.await_pending_checkpoint().await?;
        if let Some(required) = self.state.required_durable_revision {
            let durable = self.state.durable_revision.unwrap_or(0);
            if durable < required {
                return Err(AgentError::RecoveryRequired(format!(
                    "the resume checkpoint for revision {required} never landed durably \
                     (durable revision {durable}); continuation is fenced until a retry succeeds"
                )));
            }
        }
        Ok(())
    }

    /// Final barrier for a durable completion: wait out any in-flight
    /// write, capture and write one final snapshot at the current anchor
    /// revision, then wait for its acknowledgement. Returns an error when
    /// the last write failed so the caller can surface uncertainty instead
    /// of claiming resumability.
    pub(super) async fn durable_final_checkpoint(&mut self) -> AgentResult<()> {
        self.await_pending_checkpoint().await?;
        let revision = self.current_anchor_revision();
        self.schedule_checkpoint_write(revision).await;
        self.await_pending_checkpoint().await?;
        if self.state.checkpoint_write_failed {
            self.state.checkpoint_write_failed = false;
            return Err(AgentError::InvalidRequest(
                "the final checkpoint write did not land durably".into(),
            ));
        }
        Ok(())
    }
}

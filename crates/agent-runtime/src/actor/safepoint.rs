//! Safe-point resume commits and background checkpoint writes.
//!
//! At a fully settled tool batch (terminal settlement for every member,
//! no operation in flight) accrued checkpoint debt installs the bounded
//! `ExecutionState` into the task resume and schedules exactly one atomic
//! checkpoint write. Read-only exploration accrues nothing. A failed
//! background write keeps the debt visible and retryable; nothing may
//! claim safe resumability until a `CheckpointDurable` event lands.

use super::*;
use crate::checkpoint::{CheckpointDebtReason, CheckpointStore};

/// Normalize one background checkpoint join into a single result type.
async fn join_checkpoint_write(
    handle: tokio::task::JoinHandle<AgentResult<(u64, String)>>,
) -> AgentResult<(u64, String)> {
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

    /// Assemble the runtime checkpoint from the actor-owned planes plus
    /// the engine snapshot and authority marker. Shared by the external
    /// command path and safe-point writes; the host capability plane is
    /// merged by the instance layer above.
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
            // The actor does not own the host: the capability surface is
            // merged in by RuntimeInstance.
            capabilities: Vec::new(),
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
    async fn settle_finished_checkpoint_write(&mut self) {
        let finished = matches!(
            self.state.checkpoint_write.as_ref(),
            Some(handle) if handle.is_finished()
        );
        if !finished {
            return;
        }
        let handle = self
            .state
            .checkpoint_write
            .take()
            .expect("a finished handle is present");
        let joined = join_checkpoint_write(handle).await;
        match joined {
            Ok((bytes, artifact)) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable { bytes, artifact })
                    .await;
            }
            Err(error) => {
                // The debt reasons were already cleared when the write was
                // scheduled; re-accrue so the next safe point retries.
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
            }
        }
    }

    /// LONG-TASK SAFE POINT: the whole requested batch has terminal
    /// settlement and nothing is in flight. Debt coalesces into one
    /// candidate snapshot; several mutations in one batch produce one
    /// resume install and one write.
    pub(super) async fn safe_point_resume_commit(&mut self) {
        self.settle_finished_checkpoint_write().await;
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
        self.schedule_checkpoint_write().await;
    }

    /// Capture the current planes, serialize them, and hand one atomic
    /// write to the background. Debt clears only when a write is actually
    /// in flight or durability is impossible-by-configuration (surfaced,
    /// never silent).
    async fn schedule_checkpoint_write(&mut self) {
        let capture = self.capture_checkpoint().await;
        let snapshot = match capture {
            Ok(snapshot) => snapshot,
            Err(error) => {
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
        let byte_len = bytes.len() as u64;
        let Some(store) = self.checkpoint_store() else {
            self.state.checkpoint_debt.clear();
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
                .map(|artifact| (byte_len, artifact))
        }));
    }

    /// Barrier wait: explicit pause/suspend/completion/shutdown paths call
    /// this so they never report an outcome whose resume checkpoint is
    /// still in flight. A failed write surfaces here as
    /// `CheckpointWriteFailed` with debt re-armed.
    pub(super) async fn await_pending_checkpoint(&mut self) {
        self.settle_finished_checkpoint_write().await;
        let Some(handle) = self.state.checkpoint_write.take() else {
            return;
        };
        match handle.await {
            Ok(Ok((bytes, artifact))) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable { bytes, artifact })
                    .await;
            }
            Ok(Err(error)) => {
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
            }
            Err(join_error) => {
                self.accrue_checkpoint_debt(CheckpointDebtReason::TaskAnchorChanged);
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointWriteFailed {
                        reason: bounded_preview(
                            &format!("checkpoint write task failed: {join_error}"),
                            agent_contracts::MAX_TASK_ANCHOR_ITEM_CHARS,
                        ),
                    })
                    .await;
            }
        }
    }
}

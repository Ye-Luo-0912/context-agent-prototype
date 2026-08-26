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
//! sequence; the durable watermark advances only for it, while debt
//! accrued after capture stays alive for the next safe point.

use super::*;
use crate::checkpoint::{CheckpointDebtReason, CheckpointStore, StoredCheckpoint};

/// One background write in flight: its join handle, the snapshot sequence
/// it acknowledges, and the exact debt set it froze.
pub(super) struct InFlightCheckpoint {
    handle: tokio::task::JoinHandle<AgentResult<(u64, StoredCheckpoint)>>,
    /// The active task's anchor revision at freeze time, surfaced on the
    /// acknowledgement for observability only.
    anchor_revision: u64,
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
    fn current_anchor_revision(&self) -> u64 {
        self.state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.anchor.revision)
            .unwrap_or_default()
    }

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
            snapshot_sequence: self.state.snapshot_sequence,
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

    /// Drain one finished background write. A success advances the durable
    /// sequence watermark to the acked snapshot, retires precisely its
    /// captured debt reasons, and publishes `CheckpointDurable` carrying
    /// the identity tuple. A failure keeps every reason — including ones
    /// accrued mid-flight — and surfaces an error so barrier callers fail
    /// closed.
    async fn take_settled_checkpoint_write(&mut self) -> AgentResult<()> {
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
                // Typed retirement: subtract only this artifact's frozen set.
                let anchor_revision = in_flight.anchor_revision;
                self.state
                    .checkpoint_debt
                    .retain(|reason| !in_flight.captured_debt.contains(reason));
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision: anchor_revision,
                        checksum: stored.checksum,
                        sequence,
                    })
                    .await;
                Ok(())
            }
            Ok(Err(error)) => {
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
            Err(join_error) => {
                let error = AgentError::InvalidRequest(format!(
                    "checkpoint write task failed: {join_error}"
                ));
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
        }
    }

    async fn emit_checkpoint_write_failed(&mut self, detail: String) {
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
    /// continuation's required watermark. Debt accrued while another write
    /// was in flight stays here unretired, so the very next settled batch
    /// captures a further snapshot instead of silently assuming safety.
    pub(super) async fn safe_point_resume_commit(&mut self) {
        let _ = self.take_settled_checkpoint_write().await;
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

        // Allocate this snapshot's identity before freezing any plane so
        // the written payload embeds its own sequence.
        self.state.snapshot_sequence = self.state.snapshot_sequence.checked_add(1).expect(
            "snapshot sequence cannot overflow within any realistic run",
        );
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

        let captured_debt = self.state.checkpoint_debt.clone();
        self.schedule_checkpoint_write(sequence, anchor_revision, captured_debt)
            .await;
    }

    /// Capture the current planes under the already-allocated sequence and
    /// hand one atomic write to the background. Debt deliberately stays:
    /// only the successful acknowledgement retires exactly what this
    /// artifact froze; failures keep everything visible and retryable,
    /// including an impossible-by-configuration store.
    async fn schedule_checkpoint_write(
        &mut self,
        sequence: u64,
        anchor_revision: u64,
        captured_debt: Vec<CheckpointDebtReason>,
    ) {
        let capture = self.capture_checkpoint().await;
        let snapshot = match capture {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.emit_checkpoint_write_failed(error.to_string()).await;
                return;
            }
        };
        let bytes = match serde_json::to_vec(&snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.emit_checkpoint_write_failed(format!(
                    "checkpoint serialization failed: {error}"
                ))
                .await;
                return;
            }
        };
        let Some(store) = self.checkpoint_store() else {
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
                store.write_atomic(&bytes).await.map(|stored| (sequence, stored))
            }),
            anchor_revision,
            captured_debt,
        });
    }

    /// Barrier wait: explicit pause/suspend/completion/shutdown paths call
    /// this so they never report an outcome whose resume checkpoint is
    /// still in flight. A durable ack advances the sequence watermark;
    /// failure surfaces here with every debt reason retained and an error
    /// return, so callers refuse to claim resumability.
    pub(super) async fn await_pending_checkpoint(&mut self) -> AgentResult<()> {
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
                let anchor_revision = in_flight.anchor_revision;
                self.state
                    .checkpoint_debt
                    .retain(|reason| !in_flight.captured_debt.contains(reason));
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::CheckpointDurable {
                        bytes: stored.bytes,
                        artifact: stored.artifact,
                        revision: anchor_revision,
                        checksum: stored.checksum,
                        sequence,
                    })
                    .await;
                Ok(())
            }
            Ok(Err(error)) => {
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
            Err(join_error) => {
                let error = AgentError::InvalidRequest(format!(
                    "checkpoint write task failed: {join_error}"
                ));
                self.emit_checkpoint_write_failed(error.to_string()).await;
                Err(error)
            }
        }
    }

    /// Continuation across a segment is allowed only when durability is
    /// fully accounted for: no outstanding typed debt, no write in flight,
    /// no failed write without a subsequent durable one, and the required
    /// snapshot sequence actually landed durably. A failed or missing
    /// safe-point write fences `continue_active_task` before any model
    /// request and stays fenced until a retry succeeds.
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
        if !self.state.checkpoint_debt.is_empty() || self.state.checkpoint_write.is_some() {
            return Err(AgentError::RecoveryRequired(
                "outstanding checkpoint debt has not been captured at a settled safe point \
                 yet; continuation is fenced until the next safe point lands"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Final barrier for a durable completion: wait out any in-flight
    /// write, freeze one more snapshot under a freshly allocated sequence,
    /// then wait for its acknowledgement. Returns an error when the last
    /// write failed so the caller can surface uncertainty instead of
    /// claiming resumability.
    pub(super) async fn durable_final_checkpoint(&mut self) -> AgentResult<()> {
        self.await_pending_checkpoint().await?;
        self.state.snapshot_sequence = self
            .state
            .snapshot_sequence
            .checked_add(1)
            .expect("snapshot sequence cannot overflow within any realistic run");
        let sequence = self.state.snapshot_sequence;
        let captured_debt = std::mem::take(&mut self.state.checkpoint_debt);
        let anchor = self.current_anchor_revision();
        self.schedule_checkpoint_write(sequence, anchor, captured_debt)
            .await;
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

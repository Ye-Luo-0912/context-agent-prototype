use super::*;

impl RuntimeActor {
    /// Install the actor-owned planes of a runtime checkpoint, but do not
    /// claim that the full runtime is restored yet. This mutation is
    /// deliberately allowed while the actor-local event/context fence is
    /// raised, but it cannot clear an unresolved Core authority fence. On
    /// success the actor remains fenced until the host applies capabilities
    /// and calls `finalize_restore`.
    pub(super) async fn prepare_restore(
        &mut self,
        checkpoint: RuntimeCheckpoint,
    ) -> AgentResult<u64> {
        self.ensure_no_active_turn()?;
        checkpoint.validate()?;
        // CorePort is private to this single actor. No other component can
        // advance the authority epoch between this prefix proof and the CAS
        // below. A late tool may append operation truth in between, which is
        // safe: ancestor validation permits the append, and the epoch bump
        // fences that result before restored state becomes visible.
        self.validate_restore_authority(&checkpoint)?;

        // Restore may load an older checkpoint into a still-running actor.
        // Treat the restored focus as a new epoch so source revisions never
        // move backwards or alias a surface prepared before the restore.
        let restored_focus_revision = self
            .state
            .focus_revision
            .max(checkpoint.focus_revision)
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("runtime focus revision is exhausted".into()))?;
        let RuntimeCheckpoint {
            mut tasks,
            current_task_id,
            focus_revision,
            last_surface_revision,
            context,
            capabilities: _,
            authority: _,
            run_metadata,
            version,
        } = checkpoint;

        let mut restored_requirement_high_water = self.state.task_requirement_high_water.clone();
        for task in self.state.tasks.list_records() {
            restored_requirement_high_water
                .entry(task.id)
                .and_modify(|revision| {
                    *revision = (*revision).max(task.tool_requirements.revision);
                })
                .or_insert(task.tool_requirements.revision);
        }

        // Record which task revisions had to move past a live-process CAS
        // high-water mark. The event sample stays bounded.
        let mut rebased_tasks = 0usize;
        let mut rebased_task_sample: Vec<TaskId> = Vec::new();
        for task in &mut tasks.tasks {
            if let Some(live_revision) = restored_requirement_high_water.get(&task.id).copied()
                && live_revision >= task.tool_requirements.revision
            {
                task.tool_requirements.revision =
                    live_revision.checked_add(1).ok_or_else(|| {
                        AgentError::Internal(format!(
                            "task {} tool-requirement revision is exhausted",
                            task.id
                        ))
                    })?;
                rebased_tasks += 1;
                if rebased_task_sample.len() < 16 {
                    rebased_task_sample.push(task.id);
                }
            }
            restored_requirement_high_water.insert(task.id, task.tool_requirements.revision);
        }

        let old_focus_revision = self.state.focus_revision;
        let old_surface_revision = self.state.last_surface_revision;
        // Fence every pre-restore operation before any restored plane becomes
        // visible. Consuming one epoch when context restore later fails is
        // safe; installing restored state before a failed fence is not.
        let restore_id = self.bump_generation()?;
        // Durability watermarks are process-local: the loaded checkpoint
        // is itself this segment's durability proof, so pre-restore
        // bookkeeping must not fence continuation after a restore.
        self.state.resume_state_revision = None;
        self.state.required_durable_revision = None;
        self.state.durable_revision = None;
        self.core
            .restore(context, current_task_id)
            .await
            .map_err(|error| self.context_transition_failed(error))?;

        // Context and task authority become visible together. The host
        // capability plane is still outstanding, so keep the recovery fence
        // raised and retain the event fields for finalization.
        self.state.tasks.restore(tasks);
        self.state.task_id = current_task_id;
        self.state.last_assistant_artifact = None;
        self.state.task_requirement_high_water = restored_requirement_high_water;
        self.state.focus_revision = restored_focus_revision;
        self.state.last_surface_revision =
            self.state.last_surface_revision.max(last_surface_revision);
        self.state.recovery_required = true;
        self.state.pending_restore = Some(PendingRestore {
            restore_id,
            checkpoint_version: version,
            restored_run_id: run_metadata.run_id,
            focus_revision: RestoreRevision {
                old: old_focus_revision,
                restored: focus_revision,
                effective: restored_focus_revision,
            },
            surface_revision: RestoreRevision {
                old: old_surface_revision,
                restored: last_surface_revision,
                effective: self.state.last_surface_revision,
            },
            rebased_tasks,
            rebased_task_sample,
        });
        Ok(restore_id)
    }

    /// Prove that a checkpoint belongs to the live Core authority lineage
    /// before any epoch, context, task, or capability-plane mutation. A
    /// marker is an ancestor cross-check only: its state is never installed
    /// into Core. Ephemeral checkpoints cannot prove cross-process lineage,
    /// so they are restricted to the same live run.
    pub(super) fn validate_restore_authority(
        &self,
        checkpoint: &RuntimeCheckpoint,
    ) -> AgentResult<()> {
        if let AuthorityRecoveryStatus::RecoveryRequired { reason } = self.core.recovery_status() {
            return Err(AgentError::RecoveryRequired(format!(
                "Core authority must be reconciled before runtime restore: {reason}"
            )));
        }
        let live_authority = self.core.authority_checkpoint_marker()?;
        match (&checkpoint.authority, live_authority) {
            (Some(marker), Some(_)) => self.core.validate_authority_checkpoint_marker(marker),
            (Some(_), None) => Err(AgentError::RecoveryRequired(
                "checkpoint requires durable Core authority, but this runtime has no operation journal"
                    .into(),
            )),
            (None, Some(_)) => Err(AgentError::InvalidRequest(
                "checkpoint omits the durable authority marker required by this runtime".into(),
            )),
            (None, None) if checkpoint.run_metadata.run_id == self.core.run_id() => Ok(()),
            (None, None) => Err(AgentError::InvalidRequest(format!(
                "ephemeral checkpoint from run {} has no durable authority marker and cannot restore into run {}",
                checkpoint.run_metadata.run_id,
                self.core.run_id()
            ))),
        }
    }

    /// Finish a prepared restore after the host has applied capability
    /// state. The durable record is the commit point for the whole runtime;
    /// any failure leaves both the pending marker and recovery fence intact.
    pub(super) async fn finalize_restore(
        &mut self,
        restore_id: u64,
        capabilities_applied: bool,
    ) -> AgentResult<()> {
        self.ensure_no_active_turn()?;
        let pending = self.state.pending_restore.as_ref().ok_or_else(|| {
            AgentError::InvalidRequest(
                "no prepared runtime restore is awaiting finalization".into(),
            )
        })?;
        if pending.restore_id != restore_id {
            return Err(AgentError::InvalidRequest(format!(
                "stale runtime restore finalization {restore_id}; current restore is {}",
                pending.restore_id
            )));
        }
        let restored_event = RuntimeEvent::RuntimeRestored {
            checkpoint_version: pending.checkpoint_version,
            restored_run_id: pending.restored_run_id,
            current_run_id: self.core.run_id(),
            focus_revision: pending.focus_revision.clone(),
            surface_revision: pending.surface_revision.clone(),
            rebased_tasks: pending.rebased_tasks,
            rebased_task_sample: pending.rebased_task_sample.clone(),
            capabilities_applied,
        };
        match self.core.emit_event_durable(restored_event).await {
            Ok(()) => {
                self.state.pending_restore = None;
                self.state.recovery_required = matches!(
                    self.core.recovery_status(),
                    agent_contracts::AuthorityRecoveryStatus::RecoveryRequired { .. }
                );
                Ok(())
            }
            Err(error) => {
                // Do not consume pending metadata: an operator may repair
                // persistence and retry finalization, or start a new
                // known-good restore. Normal mutation stays fenced.
                self.state.recovery_required = true;
                let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                Err(error)
            }
        }
    }
}

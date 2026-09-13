use super::*;
use agent_contracts::ContextItemId;

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
        // EXEC-10 (R3-09): a parked commit transaction (an explicit
        // completion's terminal freeze waiting on its checkpoint maintenance,
        // or a safe-point prepare carrying frozen debt) is isolated from
        // restore by REFUSAL: installing restored planes under it would let
        // the old TaskTxn/prepared Context commit or roll back over a state
        // they never saw. The refusal is deterministic and typed; the
        // completion settles (or its caller retries) before any restore is
        // accepted.
        if self.state.checkpoint_prepare.is_some()
            || self
                .state
                .gc_work
                .as_ref()
                .is_some_and(|pending| pending.is_commit_in_flight())
        {
            return Err(AgentError::InvalidRequest(
                "a completion / checkpoint commit is settling; restore is refused until it \
                 settles so the old transaction cannot touch the restored state"
                    .into(),
            ));
        }
        checkpoint.validate()?;
        // EXEC-6 (R2-02): the decoded, validated checkpoint is at hand — take
        // its typed sealed references now. Finalization never re-reads or
        // scans any payload. A new restore also resets the previous run's
        // degradation facts; they are re-derived from this restore's
        // admission outcome, never accumulated across restores.
        let protected_runs = protected_runs_from_checkpoint(&checkpoint);
        self.state.restore_evidence_degraded = Vec::new();
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
            snapshot_sequence: restored_snapshot_sequence,
            capability_generation: _,
            unresolved_ack_debts: restored_ack_debts,
            event_cover_seq: _,
            terminal_commit: _,
        } = checkpoint;
        // Install the debts immediately: any restore failure path fences
        // mutation anyway, and finalization runs the reconciliation pass.
        self.state.unresolved_ack_debts = restored_ack_debts;

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
        // bookkeeping must not fence continuation after a restore. The
        // snapshot-sequence allocator is the one exception — it is lineage
        // identity, so a continued run adopts at least the restored value
        // and can never move backwards across a task switch or cold load.
        self.state.snapshot_sequence = self.state.snapshot_sequence.max(restored_snapshot_sequence);
        self.state.required_sequence = None;
        self.state.durable_sequence = None;
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
            protected_runs,
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
        let restored_run_id = pending.restored_run_id;
        // EXEC-6 (R2-02): taken from the pending borrow up front; the pending
        // slot itself is consumed below once the restore commits.
        let protected_runs = pending.protected_runs.clone();
        // EXEC-8 (R2-09): the restored run's durable journal partition stays
        // reachable for cold completion lookups. Bounded: a long restore
        // chain keeps the most recent ancestors.
        if !self.state.journal_runs.contains(&restored_run_id) {
            const MAX_JOURNAL_RUNS: usize = 64;
            if self.state.journal_runs.len() >= MAX_JOURNAL_RUNS {
                self.state.journal_runs.remove(0);
            }
            self.state.journal_runs.push(restored_run_id);
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
                // Persisted ACK debts re-fence mutation until each one is
                // reconciled against the broker journal's durable truth.
                if !self.state.unresolved_ack_debts.is_empty() {
                    self.state.recovery_required = true;
                    let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                    self.reconcile_restored_ack_debts().await;
                }
                // The restore transaction is committed. Converge the store
                // with the restored checkpoint's external map before the
                // runtime serves again — reconcile is the crash-recovery
                // authority over formal blobs (a missing store dir
                // reconciles as empty). Every still-retained checkpoint is
                // an allowed restore root, so its external blobs are strong
                // recovery roots this reconcile must not delete even when a
                // newer snapshot made the id resident (R03): a later
                // restore to that older checkpoint may still fetch them.
                // A failure is surfaced as an observable warning, never used
                // to roll the committed restore back.
                // EXEC-6 (R2-02): storage protection (context recovery
                // roots over every retained checkpoint) and read
                // authorization (the restored checkpoint's own typed
                // references) are different sets from different evidence.
                let (recovery_roots, roots_complete) =
                    self.collect_checkpoint_recovery_roots().await;
                if let Err(error) = self
                    .services
                    .context_reconcile_store_protecting(&recovery_roots, roots_complete)
                    .await
                {
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Warning {
                            message: format!("store reconcile after restore failed: {error}"),
                        })
                        .await;
                }

                // CORE-3: the restored task keeps reading the sealed
                // snapshots it captured before the restart. Model-visible
                // references (spill cursors, artifact.read pointers) name
                // the predecessor run, so admit it — and the lineage it
                // itself restored from — into this run's artifact lineage.
                // Admission failure is a visible warning: reads keep
                // failing closed, never open.
                if let Some(workspace) = self.services.artifact_workspace() {
                    let current_run = self.core.run_id();
                    match workspace
                        .admit_artifact_run_lineage(current_run, restored_run_id, &protected_runs)
                        .await
                    {
                        Ok(admission) => {
                            if !admission.unadmitted.is_empty() {
                                // Typed degradation: the restore succeeded,
                                // but the bounded lineage could not carry
                                // every protected reference. The named
                                // runs' sealed reads keep failing closed.
                                let unadmitted_runs = admission
                                    .unadmitted
                                    .iter()
                                    .map(RunId::to_string)
                                    .collect::<Vec<_>>();
                                // EXEC-6: the degradation is a queryable
                                // fact, not only a fire-once event — the
                                // typed status snapshot re-serves it until
                                // the next restore recomputes it.
                                self.state.restore_evidence_degraded = unadmitted_runs.clone();
                                let _ = self
                                    .core
                                    .emit_event(RuntimeEvent::RestoreEvidenceDegraded {
                                        unadmitted_runs,
                                    })
                                    .await;
                            }
                        }
                        Err(error) => {
                            // The whole admission failed: every protected
                            // reference is unreadable, stated as facts.
                            let unadmitted_runs = protected_runs
                                .iter()
                                .map(RunId::to_string)
                                .collect::<Vec<_>>();
                            self.state.restore_evidence_degraded = unadmitted_runs.clone();
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::RestoreEvidenceDegraded {
                                    unadmitted_runs,
                                })
                                .await;
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::Warning {
                                    message: format!(
                                        "artifact run-lineage admission failed: {error}"
                                    ),
                                })
                                .await;
                        }
                    }
                }
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

    /// Union the external blobs every still-retained, still-restorable
    /// checkpoint references. These are strong recovery roots for the
    /// post-restore store reconcile (R03) and for Storage GC at completion
    /// boundaries (W03): a blob may back a checkpoint that is older than
    /// the one just restored, and a later restore to it must still fetch
    /// the body. Each candidate envelope is decoded and validated with the
    /// same function restore uses. The boolean reports whether the root
    /// enumeration is **complete**: a list/read/decode failure, or a
    /// listing truncated by the row cap, means an unknown owner may still
    /// exist — callers must defer physical deletion rather than treat the
    /// failure as "nothing is retained". Absent a checkpoint store the
    /// empty complete set is returned (a missing store dir reconciles as
    /// empty, and there is nothing to protect).
    ///
    /// EXEC-6 (R2-02): this collector is the STORAGE protection set only.
    /// Which ancestor runs the restored state may READ is a different,
    /// strictly smaller question, answered by `protected_runs_from_checkpoint`
    /// over the restored checkpoint itself — never by an unrelated retained
    /// checkpoint, and never by raw payload text.
    pub(super) async fn collect_checkpoint_recovery_roots(&self) -> (Vec<ContextItemId>, bool) {
        // EXEC-9 (R3-01): the one shared enumeration (see the free function
        // in `maintenance.rs`) — the actor-side wrapper exists so restore
        // and the spawned boundary can never drift apart again.
        super::maintenance::collect_checkpoint_recovery_roots_for(&self.services).await
    }

    /// Reconcile the ACK debts restored with the checkpoint against the
    /// broker journal's durable reservation records. Applied/NotApplied
    /// resolutions retire the debt (the typed settlement is now durable
    /// truth); Ambiguous resolutions keep the debt and the mutation fence.
    /// The fence clears only when every debt resolved and Core reports no
    /// other recovery requirement.
    pub(super) async fn reconcile_restored_ack_debts(&mut self) {
        let debts = std::mem::take(&mut self.state.unresolved_ack_debts);
        let mut unresolved = Vec::new();
        for debt in debts {
            match self.core.reconcile_effect(&debt) {
                Ok(Some(resolution)) => {
                    let resolved = !matches!(
                        resolution,
                        agent_contracts::EffectReconciliation::Ambiguous { .. }
                    );
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::EffectAckDebtResolved {
                            debt: debt.clone(),
                            resolution,
                        })
                        .await;
                    if !resolved {
                        unresolved.push(debt);
                    }
                }
                // No broker reservation surface, or a reconciliation error:
                // the debt's truth is unknowable, so it stays and the
                // mutation fence holds.
                _ => unresolved.push(debt),
            }
        }
        if unresolved.is_empty() {
            // The restore-time fence was entered for these debts alone; it
            // may clear only while Core itself reports no other recovery
            // requirement.
            if matches!(
                self.core.recovery_status(),
                agent_contracts::AuthorityRecoveryStatus::Ready
            ) {
                self.state.recovery_required = false;
            }
        } else {
            self.state.unresolved_ack_debts = unresolved;
        }
    }
}

/// EXEC-6 (R2-02): the predecessor runs the restored checkpoint's own typed
/// fields still name through canonical SEALED artifact locators — the
/// restored state's live sealed references (EXEC-3's protection, on honest
/// evidence). Extraction never touches raw payload bytes: it reads only
/// runtime-written locator fields (completion records, final-output refs,
/// directive body refs) of an already decoded and validated checkpoint, so
/// a string that merely appears in user/tool prose — in the context payload
/// or anywhere else — is not a captured reference and grants nothing.
/// Parsing is total (`ArtifactLocator::parse_sealed`): a value that does not
/// parse is skipped, never guessed, and no byte shape can panic extraction.
/// The set is deduplicated and capped; overflow keeps the earliest entrants,
/// mirroring the bounded lineage it feeds.
fn protected_runs_from_checkpoint(checkpoint: &RuntimeCheckpoint) -> Vec<RunId> {
    let mut runs: Vec<RunId> = Vec::new();
    fn consider(value: Option<&str>, runs: &mut Vec<RunId>) {
        const MAX_PROTECTED_RUNS: usize = 64;
        if runs.len() >= MAX_PROTECTED_RUNS {
            return;
        }
        let Some(value) = value else {
            return;
        };
        let Ok(locator) = agent_contracts::ArtifactLocator::parse_sealed(value) else {
            return;
        };
        let run = locator.run_id();
        if !runs.contains(&run) {
            runs.push(run);
        }
    }
    for task in &checkpoint.tasks.tasks {
        if let Some(directive) = &task.current_directive {
            consider(directive.input.body_ref.as_deref(), &mut runs);
        }
    }
    for record in &checkpoint.tasks.completed {
        consider(record.final_output_ref.as_deref(), &mut runs);
        for artifact in &record.artifacts {
            consider(Some(artifact.as_str()), &mut runs);
        }
    }
    runs
}

#[cfg(test)]
mod exec6_extraction_tests {
    use super::*;

    fn sealed_locator(run: RunId, owner: &str) -> String {
        format!(
            "artifact://v1/{run}/{owner}/{}",
            agent_contracts::ContentDigest::sha256_bytes(owner.as_bytes())
        )
    }

    fn empty_checkpoint() -> RuntimeCheckpoint {
        crate::checkpoint::RuntimeCheckpoint {
            version: crate::checkpoint::RUNTIME_CHECKPOINT_VERSION,
            run_metadata: crate::checkpoint::RunMetadata {
                run_id: RunId::new(),
                created_at_ms: 1,
                provider_profile_digest: String::new(),
            },
            tasks: crate::checkpoint::TaskManagerSnapshot {
                tasks: Vec::new(),
                active: None,
                completed: Vec::new(),
            },
            current_task_id: None,
            focus_revision: 0,
            last_surface_revision: 0,
            context: serde_json::json!({}),
            capabilities: Vec::new(),
            authority: None,
            snapshot_sequence: 1,
            capability_generation: 0,
            unresolved_ack_debts: Vec::new(),
            event_cover_seq: 0,
            terminal_commit: false,
        }
    }

    fn envelope_template() -> agent_contracts::RuntimeInputEnvelope {
        agent_contracts::RuntimeInputEnvelope {
            preview: String::new(),
            input_id: None,
            task_id: None,
            turn_id: None,
            causal_parent: None,
            source: Default::default(),
            authority: Default::default(),
            kind: Default::default(),
            lifecycle: Default::default(),
            body_ref: None,
            digest: None,
            bytes: 0,
            proposal: Default::default(),
        }
    }

    fn completion_record(run: RunId, owner: &str) -> crate::task::CompletionRecord {
        crate::task::CompletionRecord {
            task_id: TaskId::new(),
            anchor_revision: 0,
            summary: "done".into(),
            completed_at_ms: 1,
            final_output_ref: None,
            final_output_digest: None,
            artifacts: vec![sealed_locator(run, owner)],
            verification_status: Default::default(),
            verification_refs: Vec::new(),
            disposition: Default::default(),
            unmet_reasons: Vec::new(),
        }
    }

    /// EXEC-6 (R2-02): the run ids a restored checkpoint still references
    /// through canonical SEALED locators in its runtime-written fields are
    /// exactly the protected set. Needle-form pseudo references (the raw
    /// scan's only evidence) protect nothing, and duplicates collapse.
    #[test]
    fn typed_sealed_locators_in_runtime_fields_are_the_protection_evidence() {
        let artifact_run = RunId::new();
        let output_run = RunId::new();
        let directive_run = RunId::new();
        let mut checkpoint = empty_checkpoint();

        checkpoint
            .tasks
            .completed
            .push(completion_record(artifact_run, "grep"));
        let mut final_record = completion_record(output_run, "assistant-response");
        final_record.artifacts.clear();
        final_record.final_output_ref = Some(sealed_locator(output_run, "assistant-response"));
        checkpoint.tasks.completed.push(final_record);
        // Dedup: naming the same run twice keeps one entry.
        checkpoint.tasks.completed[0]
            .artifacts
            .push(sealed_locator(artifact_run, "other-owner"));

        let directive_input = agent_contracts::RuntimeInputEnvelope {
            preview: "continue".into(),
            body_ref: Some(sealed_locator(directive_run, "user-input")),
            bytes: 8,
            ..envelope_template()
        };
        checkpoint
            .tasks
            .tasks
            .push(crate::checkpoint::TaskRecordSnapshot {
                id: TaskId::new(),
                goal: "g".into(),
                status: crate::task::TaskStatus::Active,
                created_at_ms: 0,
                last_active_ms: 0,
                tool_requirements: Default::default(),
                anchor: Default::default(),
                resume: Default::default(),
                turn_intent: String::new(),
                current_directive: Some(crate::TaskDirective {
                    input: directive_input,
                    inline_body: None,
                }),
            });

        let runs = protected_runs_from_checkpoint(&checkpoint);
        assert_eq!(
            runs.len(),
            3,
            "one protected entry per referenced run: {runs:?}"
        );
        assert!(runs.contains(&artifact_run));
        assert!(runs.contains(&output_run));
        assert!(runs.contains(&directive_run));
    }

    /// EXEC-6 (R2-02): strings that merely appear in payloads — prose in the
    /// opaque context blob, needle-form pseudo locators, draft locators,
    /// malformed ids — are not captured references and protect nothing. A
    /// multi-byte character anywhere can no longer panic extraction: the
    /// extractor never slices raw bytes.
    #[test]
    fn pseudo_references_and_malformed_unicode_protect_nothing() {
        let real_run = RunId::new();
        let mut checkpoint = empty_checkpoint();

        // The context payload is opaque engine state: even a canonical
        // locator inside it is prose, not a typed captured reference.
        checkpoint.context = serde_json::json!({
            "records": [{
                "body": format!("see artifact://v1/{real_run}/grep/{} and more", "b".repeat(64)),
                "tail": "artifact://run/汉汉汉",
            }]
        });
        // Needle-form pseudo refs (what the raw scan matched) in runtime
        // fields protect nothing: no digest, wrong shape, unparseable.
        let mut record = completion_record(real_run, "grep");
        record.artifacts = vec![
            format!("artifact://run/{real_run}/proof/aa"),
            "artifact://run/not-a-uuid".into(),
            "".to_string(),
        ];
        checkpoint.tasks.completed.push(record);

        let runs = protected_runs_from_checkpoint(&checkpoint);
        assert!(
            runs.is_empty(),
            "pseudo references must never widen the restored read set: {runs:?}"
        );

        // The exact R2-02 payload — needle + 35 ASCII + a multi-byte char —
        // rides along in an adjacent runtime field without panicking
        // anything, while the well-formed locator in its own entry still
        // parses and protects.
        let nasty = format!("{}{}", "a".repeat(35), "\u{6c49}");
        checkpoint.tasks.completed[0]
            .artifacts
            .push(format!("annotated artifact://run/{nasty} tail"));
        checkpoint.tasks.completed[0]
            .artifacts
            .push(sealed_locator(real_run, "grep"));
        let runs = protected_runs_from_checkpoint(&checkpoint);
        assert_eq!(runs, vec![real_run]);
    }

    /// EXEC-6 (R2-02): the protection set stays bounded — extraction keeps
    /// the earliest entrants past the cap instead of growing with the
    /// checkpoint.
    #[test]
    fn the_protection_set_is_bounded() {
        let mut checkpoint = empty_checkpoint();
        let mut expected = Vec::new();
        for index in 0..70 {
            let run = RunId::new();
            if expected.len() < 64 {
                expected.push(run);
            }
            checkpoint
                .tasks
                .completed
                .push(completion_record(run, &format!("owner-{index}")));
        }
        let runs = protected_runs_from_checkpoint(&checkpoint);
        assert_eq!(runs.len(), 64);
        assert_eq!(runs, expected);
    }
}

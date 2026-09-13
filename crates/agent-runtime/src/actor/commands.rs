use super::*;

impl RuntimeActor {
    pub(super) async fn process(
        &mut self,
        command: RuntimeCommand,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        match command {
            RuntimeCommand::Start { reply } => {
                let _ = reply.send(self.start_serving().await);
            }
            RuntimeCommand::UserMessage { content, reply } => {
                self.start_turn(content, reply, op_tx).await;
            }
            RuntimeCommand::SetFocus { goal, reply } => {
                // A task is the long-lived entity; focus is the attention
                // inside it. The shared `apply_focus` transition keeps the
                // TaskManager commit sequenced after the engine's focus
                // change so the two can never diverge.
                let result = match self.ensure_idle() {
                    Err(error) => Err(error),
                    Ok(()) => self.apply_focus(goal).await.map(|_| ()),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::StartWork {
                goal,
                client_request_id,
                reply,
            } => {
                match self
                    .start_work(goal.clone(), client_request_id.clone(), op_tx)
                    .await
                {
                    Ok(submission)
                        if submission.disposition
                            == crate::work::WorkSubmissionDisposition::Accepted =>
                    {
                        let record = crate::work::WorkSubmissionRecord {
                            payload_digest: crate::work::submission_payload_digest(&goal),
                            goal,
                            client_request_id,
                            task_id: submission.task_id,
                        };
                        self.set_turn_start_reply(maintenance::TurnStartReply::Work(
                            reply, submission, record,
                        ));
                    }
                    result => {
                        let _ = reply.send(result);
                    }
                }
            }
            RuntimeCommand::ActivateTask { task_id, reply } => {
                let result = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => match self.state.tasks.prepare_activate(task_id) {
                        None => Err(AgentError::InvalidRequest(format!(
                            "task {task_id} does not exist or is completed"
                        ))),
                        Some(txn) => {
                            let goal = self
                                .state
                                .tasks
                                .get(task_id)
                                .map(|task| task.goal.clone())
                                .unwrap_or_default();
                            let event_goal = goal.clone();
                            match self.bump_generation() {
                                Err(error) => Err(error),
                                Ok(_) => match self.services.set_focus(task_id, goal).await {
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
                                        .await
                                    }
                                    Err(error) => Err(self.context_transition_failed(error)),
                                },
                            }
                        }
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::SuspendTask { reply } => {
                let result = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => match self.state.tasks.prepare_suspend() {
                        None => Ok(()),
                        Some(txn) => match self.bump_generation() {
                            Err(error) => Err(error),
                            Ok(_) => match self.services.clear_focus().await {
                                Ok(report) => {
                                    self.state.tasks.commit(txn);
                                    self.state.task_id = None;
                                    self.state.last_assistant_artifact = None;
                                    self.state.focus_revision = next_focus_revision;
                                    self.publish_context_transition(
                                        RuntimeEvent::FocusCleared,
                                        ContextMaintenanceTrigger::FocusChanged,
                                        report,
                                    )
                                    .await
                                }
                                Err(error) => Err(self.context_transition_failed(error)),
                            },
                        },
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::ListTasks { reply } => {
                let _ = reply.send(Ok(self.state.tasks.list()));
            }
            RuntimeCommand::TaskPlanView { reply } => {
                let view = self
                    .state
                    .task_id
                    .and_then(|task_id| self.state.tasks.get(task_id))
                    .map(|task| crate::task::task_anchor_view(&task.anchor));
                let _ = reply.send(Ok(view));
            }
            RuntimeCommand::ReplaceTaskToolRequirements {
                task_id,
                base_revision,
                entries,
                reply,
            } => {
                let result = match self.ensure_idle() {
                    Err(error) => Err(error),
                    Ok(()) => {
                        self.set_task_tool_requirements(task_id, base_revision, entries)
                            .await
                    }
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::UpdateTaskAnchor {
                task_id,
                base_revision,
                anchor,
                reply,
            } => {
                let result = match self.ensure_idle() {
                    Err(error) => Err(error),
                    Ok(()) => match self.state.tasks.prepare_replace_anchor(
                        task_id,
                        base_revision,
                        anchor,
                    ) {
                        Err(error) => Err(error),
                        Ok((txn, revision, changed_fields)) => {
                            if changed_fields.is_empty() {
                                // Equivalent anchor: idempotent, no change
                                // event, no generation bump.
                                self.state.tasks.commit(txn);
                                Ok(revision)
                            } else {
                                let patch_kind = changed_fields_kind(&changed_fields);
                                match self.bump_generation() {
                                    Err(error) => Err(error),
                                    Ok(_) => {
                                        match self
                                            .core
                                            .emit_event(RuntimeEvent::TaskAnchorChanged {
                                                task_id,
                                                revision,
                                                changed_fields,
                                                patch_kind,
                                            })
                                            .await
                                        {
                                            Err(error) => Err(error),
                                            Ok(()) => {
                                                self.state.tasks.commit(txn);
                                                Ok(revision)
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::PatchTaskAnchor {
                task_id,
                base_revision,
                patch,
                reply,
            } => {
                let result = match self.ensure_idle() {
                    Err(error) => Err(error),
                    Ok(()) => {
                        match self
                            .state
                            .tasks
                            .prepare_patch_anchor(task_id, base_revision, &patch)
                        {
                            Err(error) => Err(error),
                            Ok((txn, revision, changed_fields, kind)) => {
                                if changed_fields.is_empty() {
                                    // Equivalent patch: idempotent, no change
                                    // event, no generation bump.
                                    self.state.tasks.commit(txn);
                                    Ok(revision)
                                } else {
                                    // Boundary patches touch user authority
                                    // (goal / constraints / waiver) and must
                                    // clear the approval gate first; autonomous
                                    // patches apply directly.
                                    if kind == AnchorPatchKind::Boundary
                                        && let Err(error) =
                                            self.authorize_anchor_patch(&patch).await
                                    {
                                        Err(error)
                                    } else {
                                        match self.bump_generation() {
                                            Err(error) => Err(error),
                                            Ok(_) => {
                                                match self
                                                    .core
                                                    .emit_event(RuntimeEvent::TaskAnchorChanged {
                                                        task_id,
                                                        revision,
                                                        changed_fields,
                                                        patch_kind: kind,
                                                    })
                                                    .await
                                                {
                                                    Err(error) => Err(error),
                                                    Ok(()) => {
                                                        self.state.tasks.commit(txn);
                                                        self.accrue_checkpoint_debt(
                                                            crate::checkpoint::CheckpointDebtReason::TaskAnchorChanged,
                                                        );
                                                        Ok(revision)
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Pin { content, reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => {
                        if content.chars().count() > MAX_PINNED_CONTENT_CHARS {
                            Err(AgentError::InvalidRequest(format!(
                                "pinned content is {} chars, above the {MAX_PINNED_CONTENT_CHARS} cap",
                                content.chars().count()
                            )))
                        } else {
                            let event_content = content.clone();
                            match self.services.pin(content).await {
                                Ok(report) => {
                                    self.publish_context_transition(
                                        RuntimeEvent::Pinned {
                                            content: event_content,
                                        },
                                        ContextMaintenanceTrigger::FocusChanged,
                                        report,
                                    )
                                    .await
                                }
                                Err(error) => Err(self.context_transition_failed(error)),
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::CompleteTask { summary, reply } => {
                // EXEC-7 (R2-08): a settled commit comes back with the reply
                // channel owned; a parked one hands the reply to the resume,
                // which settles it after the spawned checkpoint maintenance.
                let outcome = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => {
                        self.commit_completion(
                            CompletionIntent::ExplicitOperator,
                            summary,
                            Vec::new(),
                            next_focus_revision,
                            Some(reply),
                        )
                        .await
                    }
                    Err(error) => {
                        crate::actor::turn::CompletionCommitOutcome::Settled(Err(error), None)
                    }
                };
                if let crate::actor::turn::CompletionCommitOutcome::Settled(result, Some(reply)) =
                    outcome
                {
                    let _ = reply.send(result);
                }
            }
            RuntimeCommand::Checkpoint { reply } => {
                // EXEC-7 (R2-08): the capture's maintenance parks the reply
                // on the boundary lane, so a slow engine never blocks the
                // actor's other commands while the reply still carries the
                // same authoritative snapshot.
                match self.ensure_idle() {
                    Ok(()) => self.begin_read_only_capture(reply).await,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            RuntimeCommand::ContinueActiveTask { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => self.continue_active_task_turn(op_tx).await,
                    Err(error) => Err(error),
                };
                match result {
                    Ok(task_id) => self.set_turn_start_reply(
                        maintenance::TurnStartReply::Continue(reply, task_id),
                    ),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            RuntimeCommand::StatusSnapshot { reply } => {
                // Read-only, like ListTasks: no lifecycle or busy fence, so
                // recovery tooling and slow clients can always observe the
                // exact current truth.
                let focus = self.state.task_id;
                let focused = focus.and_then(|task_id| self.state.tasks.get(task_id));
                let snapshot = crate::work::RuntimeStatusSnapshot {
                    run_id: self.core.run_id(),
                    serving: self.state.lifecycle == ActorLifecycle::Serving,
                    watermark: self.core.event_sequence(),
                    focus_task_id: focus,
                    focus_goal: focused.map(|task| task.goal.clone()).unwrap_or_default(),
                    focus_anchor_revision: focused.map(|task| task.anchor.revision).unwrap_or(0),
                    tasks: self.state.tasks.list(),
                    task_hot_state: self.state.tasks.hot_state_summary(),
                    restore_evidence_degraded: self.state.restore_evidence_degraded.clone(),
                    store_backpressure: self.state.store_backpressure.clone(),
                };
                let _ = reply.send(Ok(snapshot));
            }
            RuntimeCommand::PrepareRestore { checkpoint, reply } => {
                let result = match self.ensure_serving() {
                    Ok(()) => self.prepare_restore(checkpoint).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::FinalizeRestore {
                restore_id,
                capabilities_applied,
                reply,
            } => {
                let result = match self.ensure_serving() {
                    Ok(()) => {
                        self.finalize_restore(restore_id, capabilities_applied)
                            .await
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::EmitDiagnostics { reply } => {
                // Diagnostics persist events, so they must follow the startup
                // format marker even though the engine query itself is read-only.
                let result = match self.ensure_serving() {
                    Ok(()) => self.core.emit_diagnostics().await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::InspectContext { limit, reply } => {
                let _ = reply.send(self.services.inspect_context(limit).await);
            }
            RuntimeCommand::TaskCompletionLookup { task_id, reply } => {
                // EXEC-8 (R2-09): read-only, no fences that could starve —
                // the hot check is one table scan; the cold path reads a
                // bounded tail of the durable journal. No model, no tool,
                // no checkpoint write.
                if self.state.tasks.get(task_id).is_some() {
                    let _ = reply.send(Ok(crate::work::TaskCompletionLookup::Hot));
                    return;
                }
                // EXEC-8 残余 (R3-11): the cold scan runs on its own task —
                // the actor's command branch never waits on journal I/O, so
                // status/cancel/stop stay live during a long scan.
                let journal = self.services.event_journal();
                let runs = {
                    let mut runs = vec![self.core.run_id()];
                    runs.extend(self.state.journal_runs.iter().copied().rev());
                    runs
                };
                tokio::spawn(async move {
                    let result =
                        crate::work::cold_completion_lookup_in(&journal, &runs, task_id).await;
                    let _ = reply.send(result);
                });
            }
            RuntimeCommand::TaskDetail { task_id, reply } => {
                // Read-only, like StatusSnapshot: no idle fence, no model
                // round, no checkpoint. Unknown tasks are a typed error.
                let result = match self.state.tasks.get(task_id) {
                    Some(task) => Ok(crate::work::TaskDetailSnapshot {
                        task: crate::task::TaskInfo {
                            id: task.id,
                            goal: task.goal.clone(),
                            status: task.status,
                            tool_requirement_revision: task.tool_requirements.revision,
                            tool_requirement_count: task.tool_requirements.entries.len(),
                            anchor_revision: task.anchor.revision,
                        },
                        anchor: crate::task::task_anchor_view(&task.anchor),
                    }),
                    None => Err(AgentError::InvalidRequest(format!(
                        "task not found: {task_id}"
                    ))),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::QueryWorkSubmission {
                client_request_id,
                payload_digest,
                reply,
            } => {
                // Read-only, like TaskDetail: no idle fence, no model round,
                // no checkpoint. The ledger is bounded and process-local, so
                // an id outside it is `Unknown` — the honest fact, never a
                // claim that the request was not executed.
                let query = match self
                    .state
                    .work_submissions
                    .iter()
                    .find(|record| record.client_request_id == client_request_id)
                {
                    Some(record) => crate::work::WorkSubmissionQuery::Recorded {
                        task_id: record.task_id,
                        payload_digest: record.payload_digest.clone(),
                        matches: payload_digest
                            .as_deref()
                            .map(|asked| asked == record.payload_digest),
                    },
                    None => crate::work::WorkSubmissionQuery::Unknown,
                };
                let _ = reply.send(Ok(query));
            }
            RuntimeCommand::QueryOperation {
                operation_id,
                reply,
            } => {
                // Authority queries are intentionally available while the
                // runtime is fenced: recovery tooling needs the exact truth
                // in order to decide whether mutation may resume.
                let _ = reply.send(Ok(self.core.query_operation(operation_id)));
            }
            RuntimeCommand::CancelOperation { identity, reply } => {
                let result = match self.ensure_serving() {
                    Ok(()) => self.cancel_operation(identity).await,
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.drain_queued_user_input(op_tx).await;
                }
                let _ = reply.send(result);
            }
            RuntimeCommand::CancelTurn { reply } => {
                let result = match self.ensure_serving() {
                    Ok(()) => {
                        self.cancel_turn(TurnCancellationReason::Requested, None)
                            .await
                    }
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.drain_queued_user_input(op_tx).await;
                }
                let _ = reply.send(result);
            }
            RuntimeCommand::Stop { .. } => unreachable!("Stop is handled in the run loop"),
        }
    }
}

impl RuntimeActor {}

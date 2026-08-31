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
                let result = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => {
                        // A task is the long-lived entity; focus is the
                        // attention inside it. `prepare_create` resumes a
                        // non-completed task with the same goal, so
                        // re-focusing returns to the same task id. The
                        // TaskManager transition is committed only after
                        // the engine's focus change succeeded, so the two
                        // can never diverge. An oversized goal or a
                        // saturated catalog fails closed here, before any
                        // engine or event mutation.
                        match self.state.tasks.prepare_create(&goal) {
                            Err(error) => Err(error),
                            Ok((txn, task_id)) => {
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
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
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
            RuntimeCommand::ReplaceTaskToolRequirements {
                task_id,
                base_revision,
                entries,
                reply,
            } => {
                let result = match self.ensure_idle() {
                    Err(error) => Err(error),
                    Ok(()) => match normalize_tool_requirements(entries) {
                        Err(error) => Err(error),
                        Ok(entries) => {
                            match self.state.tasks.prepare_replace_tool_requirements(
                                task_id,
                                base_revision,
                                entries.clone(),
                            ) {
                                Err(error) => Err(error),
                                Ok((txn, revision)) => {
                                    let changed = revision != base_revision;
                                    if changed {
                                        match self.bump_generation() {
                                            Err(error) => Err(error),
                                            Ok(_) => {
                                                match self
                                                    .core
                                                    .emit_event(
                                                        RuntimeEvent::TaskToolRequirementsChanged {
                                                            task_id,
                                                            revision,
                                                            requirements: entries,
                                                        },
                                                    )
                                                    .await
                                                {
                                                    Err(error) => Err(error),
                                                    Ok(()) => {
                                                        self.state.tasks.commit(txn);
                                                        self.state
                                                            .task_requirement_high_water
                                                            .insert(task_id, revision);
                                                        Ok(revision)
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        self.state.tasks.commit(txn);
                                        Ok(revision)
                                    }
                                }
                            }
                        }
                    },
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
                let result = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => {
                        self.commit_completion(
                            CompletionIntent::ExplicitOperator,
                            summary,
                            Vec::new(),
                            next_focus_revision,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Checkpoint { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => self.capture_checkpoint().await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::ContinueActiveTask { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => self.continue_active_task_turn(op_tx).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
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

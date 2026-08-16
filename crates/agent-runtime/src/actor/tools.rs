use super::*;

impl RuntimeActor {
    /// Prepare + spawn one tool call. Core first appends the exact operation
    /// identity to its authority WAL; only then does Runtime publish
    /// `OperationAccepted` / `ToolStarted` and consume the one-shot dispatch
    /// permit. This makes the event stream a safe discovery surface without
    /// turning it into operation authority.
    pub(super) async fn spawn_tool_operation(
        &mut self,
        call: ToolCall,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        // An earlier effect in this turn may have landed without a durable
        // record or returned an unknown state. Keep the model informed by
        // completing the current turn, but never dispatch another tool while
        // recovery is required; that would build new effects on an
        // unprovable world state before PLAT-03 can arbitrate them.
        if self.state.recovery_required {
            let mut refused = vec![call];
            if let Some(turn) = self.state.turn.as_mut() {
                refused.extend(turn.pending_tools.drain(..));
            }
            for call in refused {
                let output = ToolOutput {
                    call_id: call.id,
                    tool_name: call.name,
                    ok: false,
                    summary: "tool call refused: runtime recovery is required".into(),
                    model_content: "A prior effect in this turn is not durably reconciled. No further tool was executed; finish with the known state and recover before continuing.".into(),
                    artifact_ref: None,
                    metadata: serde_json::json!({
                        "code": "runtime.recovery_required",
                        "executed": false,
                    }),
                };
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ToolFinished {
                        output: output.clone(),
                    })
                    .await;
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result(output, None);
                }
            }
            self.spawn_next_model_or_end(op_tx).await;
            return;
        }
        let mut call = call;
        loop {
            if let Some(query) = discovery_search_from_call(&call.name, &call.arguments)
                && let Err(exhausted) = self.state.discovery_budget.admit(&query)
            {
                let output = discovery_budget_refusal(&call, exhausted);
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ToolFinished {
                        output: output.clone(),
                    })
                    .await;
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result_with(
                        output,
                        None,
                        ToolResultDisposition::TransientNoPersist,
                    );
                    if let Some(next) = turn.pending_tools.pop_front() {
                        call = next;
                        continue;
                    }
                }
                self.spawn_next_model_or_end(op_tx).await;
                return;
            }
            break;
        }
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        let turn_id = turn.turn_id;
        let surface = turn.tool_surface.clone();
        let Some(surface) = surface else {
            // No round has run yet; nothing legitimately queues a tool call
            // before the first model round.
            return;
        };

        // The tool scope opens when the tool starts — it is an execution
        // frame, not a batch artifact of turn-end persistence.
        let tool_scope = match self
            .services
            .context_open_scope(ScopeKind::Tool, None)
            .await
        {
            Ok(scope) => Some(scope),
            Err(error) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
        };
        let cancel = CancellationToken::new();
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        self.state.active_tool = Some(call.name.clone());
        let core = self.core.clone();
        let op_tx = op_tx.clone();
        let run_id = core.run_id();
        let task_id = self.state.task_id;
        let argument_digest = ArgumentDigest::from_json(&call.arguments);
        let identity = ToolOperationIdentity {
            run_id,
            task_id,
            turn_id,
            scope_id: tool_scope,
            operation_id,
            generation,
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            argument_digest,
        };
        let admission = match self
            .core
            .admit_tool_operation(identity.clone(), &call, generation)
        {
            Ok(ToolOperationAdmission::Accepted { permit, .. }) => permit,
            Ok(ToolOperationAdmission::AlreadyKnown { snapshot }) => {
                self.state.active_tool = None;
                self.state.recovery_required = true;
                let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: crate::output::bound_error_message(format!(
                            "runtime generated an already-known operation id {}; Core state is {:?}",
                            identity.operation_id, snapshot.state
                        )),
                    })
                    .await;
                if let Some(scope_id) = tool_scope {
                    let _ = self.services.context_close_scope(scope_id).await;
                }
                self.state.turn = None;
                return;
            }
            Err(error) => {
                self.state.active_tool = None;
                if matches!(error, AgentError::RecoveryRequired(_)) {
                    self.state.recovery_required = true;
                    let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                }
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: crate::output::bound_error_message(format!(
                            "tool operation admission failed before dispatch: {error}"
                        )),
                    })
                    .await;
                if let Some(scope_id) = tool_scope {
                    let _ = self.services.context_close_scope(scope_id).await;
                }
                self.state.turn = None;
                return;
            }
        };
        let admitted_permit = admission;
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            kind: OpKind::Tool,
            scope_id: tool_scope,
            tool_identity: Some(identity.clone()),
            cancel: cancel.clone(),
        });
        let permit = match self
            .core
            .publish_tool_operation(admitted_permit, &call)
            .await
        {
            Ok(permit) => permit,
            Err(error) => {
                self.abort_admitted_tool_before_dispatch(
                    &identity,
                    tool_scope,
                    format!(
                        "operation {} was durably admitted, but its lifecycle publication failed: {error}",
                        identity.operation_id
                    ),
                )
                .await;
                return;
            }
        };
        let completion_identity = identity.clone();
        tokio::spawn(async move {
            let execution = core
                .execute_published_tool(permit, call, cancel, &surface)
                .await;
            let agent_core::CoreToolExecution {
                outcome,
                lease,
                effect_id,
                argument_digest,
                value_completion_pending,
            } = execution;
            let (operation, effect, directive, disposition) = match outcome {
                ToolOutcome::Value(output) => {
                    let disposition = tool_value_disposition(&output);
                    (
                        OperationResult {
                            run_id,
                            turn_id,
                            task_id,
                            scope_id: tool_scope,
                            operation_id,
                            generation,
                            outcome: OperationOutcome::ToolOutput(output),
                        },
                        None,
                        None,
                        disposition,
                    )
                }
                ToolOutcome::PreparedEffect { output, effect } => (
                    OperationResult {
                        run_id,
                        turn_id,
                        task_id,
                        scope_id: tool_scope,
                        operation_id,
                        generation,
                        outcome: OperationOutcome::ToolOutput(output),
                    },
                    Some(effect),
                    None,
                    ToolResultDisposition::PersistObservation,
                ),
                ToolOutcome::RuntimeDirective { output, directive } => {
                    // Most directives are decision records and persist as
                    // observations. `admit` re-enters the *same* item id, so
                    // persisting the result would duplicate it under a new
                    // id — the admission event is the record.
                    // `derive` already persists a new derived item via the
                    // directive; the result text stays transient.
                    let disposition = match &directive {
                        RuntimeDirective::Context(agent_contracts::ContextAction::Admit {
                            ..
                        }) => ToolResultDisposition::AccessEventOnly,
                        RuntimeDirective::Context(agent_contracts::ContextAction::Derive {
                            ..
                        }) => ToolResultDisposition::TransientNoPersist,
                        _ => ToolResultDisposition::PersistObservation,
                    };
                    (
                        OperationResult {
                            run_id,
                            turn_id,
                            task_id,
                            scope_id: tool_scope,
                            operation_id,
                            generation,
                            outcome: OperationOutcome::ToolOutput(output),
                        },
                        None,
                        Some(directive),
                        disposition,
                    )
                }
                // The tool asked the runtime to resolve a read-only engine
                // query: the kernel (the ContextEngine owner) answers and
                // the placeholder output becomes the final one. No effect,
                // no directive — search/inspect/fetch are pure reads. The
                // result is transient: reading evidence must not duplicate
                // it as a new observation.
                ToolOutcome::EngineQuery { output, query } => {
                    let resolved = core.resolve_engine_query(output, query).await;
                    (
                        OperationResult {
                            run_id,
                            turn_id,
                            task_id,
                            scope_id: tool_scope,
                            operation_id,
                            generation,
                            outcome: OperationOutcome::ToolOutput(resolved),
                        },
                        None,
                        None,
                        ToolResultDisposition::TransientNoPersist,
                    )
                }
            };
            let _ = op_tx
                .send(OperationCompletion {
                    operation,
                    kind: OpKind::Tool,
                    effect,
                    lease,
                    effect_id,
                    argument_digest: Some(argument_digest),
                    tool_identity: Some(completion_identity),
                    value_completion_pending,
                    directive,
                    disposition,
                    context_ack: None,
                })
                .await;
        });
    }

    /// Terminalize one WAL-admitted operation whose lifecycle event failed
    /// before dispatch, and close the execution frame that can no longer be
    /// reached from turn state. Every cleanup failure remains observable and
    /// recovery-fenced; dropping the one-shot permit guarantees no tool body
    /// starts afterward.
    pub(super) async fn abort_admitted_tool_before_dispatch(
        &mut self,
        identity: &ToolOperationIdentity,
        tool_scope: Option<ScopeId>,
        failure: String,
    ) {
        self.state.active_tool = None;
        self.state.recovery_required = true;
        let mut message = failure;
        let already_cancelled = matches!(
            self.core.query_operation(identity.operation_id),
            OperationQueryResult::Found { snapshot }
                if snapshot.identity == *identity
                    && matches!(
                        snapshot.state,
                        OperationState::Terminal {
                            terminal: OperationTerminal::CancelledBeforeCommit,
                            ..
                        }
                    )
        );
        if !already_cancelled && let Err(error) = self.core.cancel_operation(identity.clone()) {
            message.push_str(&format!(
                "; Core could not terminalize the admitted operation: {error}"
            ));
        }
        if let Some(scope_id) = tool_scope {
            match tokio::time::timeout(
                TOOL_SCOPE_CLOSE_TIMEOUT,
                self.services.context_close_scope(scope_id),
            )
            .await
            {
                Ok(Ok(transitions)) => {
                    if let Err(error) = self
                        .core
                        .emit_event(RuntimeEvent::ToolScopeClosed {
                            scope_id,
                            transitions,
                        })
                        .await
                    {
                        message.push_str(&format!(
                            "; closed tool scope {scope_id}, but its event failed: {error}"
                        ));
                    }
                }
                Ok(Err(error)) => message.push_str(&format!(
                    "; admitted tool scope {scope_id} could not be closed: {error}"
                )),
                Err(_) => message.push_str(&format!(
                    "; admitted tool scope {scope_id} did not close within {TOOL_SCOPE_CLOSE_TIMEOUT:?} and remains unresolved"
                )),
            }
        }
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        let _ = self
            .core
            .emit_event(RuntimeEvent::Error {
                message: crate::output::bound_error_message(message),
            })
            .await;
        self.state.turn = None;
    }

    /// Verify a finished operation still belongs to the current turn and
    /// generation. Stale completions (cancelled or superseded) are dropped
    /// and surfaced as a warning; live ones are committed. A prepared side
    /// effect follows the same fence: roll back when stale, commit when
    /// live — the tool's computation already happened, but its side effect
    /// only lands here.
    pub(super) async fn on_operation_completed(
        &mut self,
        completion: OperationCompletion,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        if self.is_stale(&completion) {
            // The operation turned stale before its side effect was
            // committed: roll the staged effect back so a cancelled or
            // superseded tool never mutates the workspace.
            if let Some(effect) = completion.effect {
                let reason = format!(
                    "stale {} result dropped (turn {}, generation {})",
                    match completion.kind {
                        OpKind::Model => "model",
                        OpKind::Tool => "tool",
                    },
                    completion.operation.turn_id,
                    completion.operation.generation
                );
                if let Err(error) = self
                    .core
                    .rollback_effect(EffectRollbackRequest {
                        run_id: completion.operation.run_id,
                        turn_id: completion.operation.turn_id,
                        operation_id: completion.operation.operation_id,
                        effect_id: completion.effect_id,
                        argument_digest: completion
                            .argument_digest
                            .unwrap_or_else(|| ArgumentDigest::sha256_bytes(&[])),
                        generation: completion.operation.generation,
                        lease: completion.lease,
                        effect,
                        reason,
                    })
                    .await
                {
                    tracing::warn!(%error, "Core rejected stale-effect rollback identity after cleanup");
                    self.state.recovery_required = true;
                    let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                } else if self.state.pending_tool_cleanup == Some(completion.operation.operation_id)
                {
                    self.state.pending_tool_cleanup = None;
                }
            } else {
                if completion.value_completion_pending
                    && let Some(identity) = completion.tool_identity.clone()
                    && let Err(error) = self.core.cancel_operation(identity)
                {
                    tracing::warn!(%error, "Core could not terminalize stale value operation");
                    self.state.recovery_required = true;
                    let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                }
                if self.state.pending_tool_cleanup == Some(completion.operation.operation_id) {
                    self.state.pending_tool_cleanup = None;
                }
            }
            let message = format!(
                "stale {} result dropped (turn {}, generation {})",
                match completion.kind {
                    OpKind::Model => "model",
                    OpKind::Tool => "tool",
                },
                completion.operation.turn_id,
                completion.operation.generation
            );
            if let Err(error) = self.core.emit_warning(message).await {
                tracing::warn!(%error, "failed to emit stale-result warning");
            }
            return;
        }

        let op_scope_id = completion.operation.scope_id;
        let context_ack = completion.context_ack;
        if let Some(turn) = self.state.turn.as_mut() {
            turn.op = None;
        }
        match completion.operation.outcome {
            OperationOutcome::ModelOutput {
                content,
                tool_calls,
                usage,
            } => {
                if let Some(ack) = context_ack
                    && let Err(error) = self.core.acknowledge_context_consumption(ack).await
                {
                    let error = self.context_transition_failed(error);
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: format!("failed to commit model context consumption: {error}"),
                        })
                        .await;
                    self.state.turn = None;
                    return;
                }
                self.emit_input_consumed().await;
                // Report the round's true provider usage to live consumers
                // (the eval harness, a token meter). Best-effort: a journal
                // failure here must not abort the turn commit.
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ModelUsed {
                        input_tokens: usage.input_tokens.unwrap_or(0),
                        output_tokens: usage.output_tokens.unwrap_or(0),
                    })
                    .await;
                if tool_calls.is_empty() {
                    self.finalize_turn(content).await;
                    self.drain_queued_user_input(op_tx).await;
                } else {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.turn_frame.push_tool_calls(tool_calls.clone());
                        turn.pending_tools.extend(tool_calls);
                    }
                    self.advance_turn(op_tx).await;
                    self.drain_queued_user_input(op_tx).await;
                }
            }
            OperationOutcome::ToolOutput(output) => {
                // The actor's current-turn/generation fence passed. Core now
                // validates the run identity and Core-issued lease itself
                // before committing; Runtime cannot bypass that check by
                // obtaining an EffectAuthority object.
                let output = match completion.effect {
                    Some(effect) => match self
                        .core
                        .commit_effect(EffectCommitRequest {
                            run_id: completion.operation.run_id,
                            turn_id: completion.operation.turn_id,
                            operation_id: completion.operation.operation_id,
                            effect_id: completion
                                .effect_id
                                .expect("prepared effects receive a Core effect id"),
                            argument_digest: completion
                                .argument_digest
                                .expect("tool operations carry an argument digest"),
                            generation: completion.operation.generation,
                            lease: completion.lease,
                            effect,
                        })
                        .await
                    {
                        EffectCommitDisposition::Receipt(EffectReceipt::Applied {
                            durability: EffectDurability::Durable,
                            ..
                        }) => output,
                        EffectCommitDisposition::Receipt(EffectReceipt::NotApplied { error }) => {
                            ToolOutput {
                                ok: false,
                                summary: format!("effect commit failed: {error}"),
                                model_content: format!(
                                    "the change was prepared but could not be committed: {error}"
                                ),
                                ..output
                            }
                        }
                        EffectCommitDisposition::Receipt(EffectReceipt::Applied {
                            durability: EffectDurability::DurabilityFailed(error),
                            ..
                        }) => {
                            // At least one side effect landed, but the
                            // operation is not durably complete (a journal
                            // failure or a partial sequential composite).
                            // Keep this turn alive long enough to tell the
                            // model the truth, while fencing every later
                            // ordinary mutation behind a known-good restore.
                            self.require_effect_recovery(format!(
                                "effect applied but recovery is required: {error}"
                            ))
                            .await;
                            ToolOutput {
                                ok: false,
                                summary: format!(
                                    "effect applied but recovery is required: {error}"
                                ),
                                model_content: format!(
                                    "at least one change WAS applied, but the effect operation did not complete durably: {error}. Recovery is required before another mutation."
                                ),
                                ..output
                            }
                        }
                        EffectCommitDisposition::Receipt(EffectReceipt::Unknown { error }) => {
                            // Retrying or accepting another mutation would
                            // build on a world whose state is unknowable.
                            // The current turn still receives this honest
                            // result and may explain it to the user.
                            self.require_effect_recovery(format!(
                                "effect applied state unknown; recovery is required: {error}"
                            ))
                            .await;
                            ToolOutput {
                                ok: false,
                                summary: format!("effect applied state unknown: {error}"),
                                model_content: format!(
                                    "the change may or may not have been applied (the applied state is unknown): {error}. It is not retried blindly, and recovery is required before another mutation."
                                ),
                                ..output
                            }
                        }
                        EffectCommitDisposition::AuthorityRecordFailed { receipt, error } => {
                            self.require_effect_recovery(format!(
                                "effect authority record failed after receipt {receipt:?}: {error}"
                            ))
                            .await;
                            match receipt {
                                EffectReceipt::NotApplied {
                                    error: effect_error,
                                } => ToolOutput {
                                    ok: false,
                                    summary: format!(
                                        "effect was not applied, but recovery is required: {effect_error}"
                                    ),
                                    model_content: format!(
                                        "the change was NOT applied, but Core could not record the terminal operation state: {error}. Recovery is required before another mutation."
                                    ),
                                    ..output
                                },
                                EffectReceipt::Applied { .. } => ToolOutput {
                                    ok: false,
                                    summary: "effect applied but authority recovery is required"
                                        .into(),
                                    model_content: format!(
                                        "the change WAS applied, but Core could not record the terminal operation state: {error}. Recovery is required before another mutation."
                                    ),
                                    ..output
                                },
                                EffectReceipt::Unknown {
                                    error: effect_error,
                                } => ToolOutput {
                                    ok: false,
                                    summary: format!("effect state unknown: {effect_error}"),
                                    model_content: format!(
                                        "the change may or may not have been applied, and Core could not record the terminal operation state: {error}. Do not retry blindly; recovery is required."
                                    ),
                                    ..output
                                },
                            }
                        }
                        EffectCommitDisposition::Rejected(rejection) => {
                            let detail = match rejection {
                                EffectCommitRejection::ForeignRun => {
                                    "the staged effect belonged to a different runtime run"
                                }
                                EffectCommitRejection::StaleEpoch => {
                                    "the operation authority epoch was stale when Core checked the commit"
                                }
                                EffectCommitRejection::MissingLease => {
                                    "the staged effect had no authorization lease"
                                }
                                EffectCommitRejection::InvalidLease => {
                                    "the authorization lease expired or did not match this operation generation"
                                }
                                EffectCommitRejection::InvalidOperation => {
                                    "Core rejected the operation or prepared-effect identity"
                                }
                            };
                            ToolOutput {
                                ok: false,
                                summary: "effect authorization rejected before commit".into(),
                                model_content: format!("the change was not applied: {detail}."),
                                ..output
                            }
                        }
                    },
                    None => output,
                };
                // Last-line invariant guard: untrusted capability/process
                // outputs and context fetches must never enter the turn
                // frame, context engine or event stream unbounded. Normal
                // tools spill before this point; this guard makes a
                // producer contract violation safe and visible.
                let output = bound_tool_output(output);
                // Execute the tool's runtime directive now, as part of the
                // operation commit — not at turn end — so a context control
                // request takes effect before the next model round.
                if let Some(directive) = completion.directive {
                    self.execute_directive(directive).await;
                }
                // The tool's bounded output names entities the next model
                // round should treat as hot, *before* the observation body
                // is persisted at turn end. The signal is a no-body, bounded
                // hot-entity extension: Warm/Cold evidence can be recalled
                // immediately without duplicating the tool body.
                let _ = self
                    .services
                    .context_ingest(ContextIngress::WorkingSetSignal {
                        content: output.working_set_signal_text(),
                    })
                    .await;
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result_with(
                        output.clone(),
                        op_scope_id,
                        completion.disposition,
                    );
                }
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ToolFinished { output })
                    .await;
                if completion.value_completion_pending
                    && let Some(argument_digest) = completion.argument_digest
                    && let Err(error) = self.core.finish_value_operation(
                        completion.operation.operation_id,
                        argument_digest,
                        completion.operation.generation,
                    )
                {
                    self.require_effect_recovery(format!(
                        "Core could not record accepted tool value completion: {error}"
                    ))
                    .await;
                }
                self.advance_turn(op_tx).await;
                self.drain_queued_user_input(op_tx).await;
            }
            OperationOutcome::Failed { message } => {
                let _ = self.core.emit_event(RuntimeEvent::Error { message }).await;
                self.state.turn = None;
                self.drain_queued_user_input(op_tx).await;
            }
            OperationOutcome::Cancelled => {
                if let Err(error) = self
                    .cancel_turn(
                        TurnCancellationReason::OperationCancelled,
                        Some(completion.operation.operation_id),
                    )
                    .await
                {
                    self.state.recovery_required = true;
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: crate::output::bound_error_message(format!(
                                "operation cancellation could not reach its durable barrier: {error}"
                            )),
                        })
                        .await;
                    let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
                }
                self.drain_queued_user_input(op_tx).await;
            }
            OperationOutcome::Completed => {
                self.state.turn = None;
                self.drain_queued_user_input(op_tx).await;
            }
        }
    }

    /// Poison the normal-mutation lane after an effect result proves that
    /// the world cannot safely be used as the base for more work. The flag
    /// is set before best-effort observability writes, so a failed warning
    /// or event append cannot accidentally leave mutation enabled. The
    /// active turn is deliberately not aborted: it must carry the truthful
    /// receipt back to the model/user and durably record that outcome.
    pub(super) async fn require_effect_recovery(&mut self, warning: String) {
        let newly_required = !self.state.recovery_required;
        self.state.recovery_required = true;
        let _ = self.core.emit_warning(warning).await;
        if newly_required {
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        }
    }

    /// An operation is stale when the turn it belongs to is gone or the
    /// in-flight identity no longer matches (a cancel or a newer turn).
    pub(super) fn is_stale(&self, completion: &OperationCompletion) -> bool {
        let Some(turn) = &self.state.turn else {
            return true;
        };
        turn.op.as_ref().is_none_or(|op| {
            op.operation_id != completion.operation.operation_id
                || op.turn_id != completion.operation.turn_id
                || op.generation != completion.operation.generation
        })
    }
}

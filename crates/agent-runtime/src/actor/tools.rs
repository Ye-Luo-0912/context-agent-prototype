use super::*;

const MAX_SETTLEMENT_DETAIL_CHARS: usize = 1_200;

/// A prepared output describes proposed bytes, not observed world state.
/// Unless commit settles as fully durable, strip every proposed revision and
/// retain only a bounded set of attempted paths. Partial/unknown receipts do
/// not yet carry the exact committed subset, so preserving `files[]` there
/// would manufacture facts for files that might never have landed.
fn unsettled_effect_output(
    mut output: ToolOutput,
    commit_state: &'static str,
    summary_prefix: &str,
    model_prefix: &str,
    detail: &str,
    diagnose: bool,
) -> ToolOutput {
    let mut attempted_paths = Vec::new();
    if let Some(path) = output.metadata.get("path").and_then(|value| value.as_str()) {
        push_attempted_path(&mut attempted_paths, path);
    }
    if let Some(files) = output
        .metadata
        .get("files")
        .and_then(|value| value.as_array())
    {
        for file in files {
            if let Some(path) = file.get("path").and_then(|value| value.as_str()) {
                push_attempted_path(&mut attempted_paths, path);
            }
        }
    }
    let detail = bounded_preview(detail, MAX_SETTLEMENT_DETAIL_CHARS);
    output.ok = false;
    output.summary = bounded_preview(
        &format!("{summary_prefix}: {detail}"),
        agent_contracts::MAX_TOOL_SUMMARY_CHARS,
    );
    output.model_content = format!("{model_prefix}: {detail}");
    output.artifact_ref = None;
    output.metadata = serde_json::json!({
        "commit_state": commit_state,
        "attempted_paths": attempted_paths,
    });
    if diagnose {
        apply_runtime_diagnosis(&mut output, None);
    }
    bound_settlement_metadata(&mut output.metadata);
    output
}

fn push_attempted_path(paths: &mut Vec<String>, path: &str) {
    if paths.len() >= agent_contracts::MAX_RESOURCE_TOUCHES {
        return;
    }
    let path = bounded_preview(path, agent_contracts::MAX_RESOURCE_PATH_CHARS);
    if !path.is_empty() && !paths.iter().any(|candidate| candidate == &path) {
        paths.push(path);
    }
}

fn bound_settlement_metadata(metadata: &mut serde_json::Value) {
    // Path caps are expressed in Unicode characters while the envelope cap
    // is serialized UTF-8 bytes. Drop the least-prioritized tail paths until
    // the actual wire representation fits; four-byte paths must not bypass
    // the broker merely because settlement happens after it.
    while serde_json::to_vec(&*metadata)
        .map(|bytes| bytes.len() > agent_contracts::MAX_TOOL_METADATA_BYTES)
        .unwrap_or(true)
    {
        let removed = metadata
            .get_mut("attempted_paths")
            .and_then(|value| value.as_array_mut())
            .and_then(Vec::pop)
            .is_some();
        if !removed {
            *metadata = serde_json::json!({
                "commit_state": "unsettled",
                "attempted_paths": [],
            });
            break;
        }
    }
}

fn not_applied_effect_output(output: ToolOutput, error: &str) -> ToolOutput {
    unsettled_effect_output(
        output,
        "not_applied",
        "effect commit failed",
        "the change was prepared but could not be committed",
        error,
        true,
    )
}

/// The dispatcher or preparation path failed while cleaning staged state.
/// Core has already installed its recovery fence and deliberately keeps the
/// operation queryable, so this projection must not preserve proposed
/// revisions or imply that any target landed.
fn execution_cleanup_recovery_output(output: ToolOutput, error: &str) -> ToolOutput {
    unsettled_effect_output(
        output,
        "execution_cleanup_recovery_required",
        "tool execution cleanup requires recovery",
        "the tool reported no committed result, but preparation cleanup could not be confirmed; recovery is required before another mutation",
        error,
        true,
    )
}

impl RuntimeActor {
    /// Combine trusted dispatcher attribution with task-owned authority at
    /// the last pre-dispatch safe point. Exact textual path mentions are
    /// allowed only from current/evolvable task fields; the historical task
    /// origin alone is not a perpetual precondition.
    fn runtime_execution_attribution(&self, call: &ToolCall) -> RuntimeExecutionAttribution {
        let host = self.services.tool_execution_attribution(call);
        let active_task = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id));
        let execution = self.state.turn.as_ref().map(|turn| &turn.execution);
        let mut rooted_targets = Vec::new();
        for target in &host.targets {
            let mutation_precondition =
                host.purpose == agent_contracts::ToolExecutionPurpose::Mutate;
            let structured = execution.is_some_and(|state| state.path_is_execution_rooted(target));
            let textual = active_task.is_some_and(|task| {
                path_exactly_in_directive(&task.turn_intent, target)
                    || path_exactly_in_directive(&task.anchor.current_interpretation, target)
                    || task
                        .anchor
                        .constraints
                        .iter()
                        .chain(task.anchor.acceptance_criteria.iter())
                        .chain(task.anchor.plan_progress.iter())
                        .chain(task.anchor.open_loops.iter())
                        .any(|item| path_exactly_in_directive(item, target))
            });
            if mutation_precondition || structured || textual {
                rooted_targets.push(target.clone());
            }
        }
        rooted_targets.sort();
        rooted_targets.dedup();
        RuntimeExecutionAttribution {
            host,
            rooted_targets,
        }
    }

    /// Exact verification identity is sampled on both sides of execution.
    /// A changed or unavailable post identity keeps the successful result as
    /// typed TaskScoped evidence but prevents no-dispatch PASS reuse.
    fn settled_execution_attribution(
        &self,
        preflight: Option<&RuntimeExecutionAttribution>,
        verification_call: Option<&ToolCall>,
        output: &mut ToolOutput,
    ) -> Option<RuntimeExecutionAttribution> {
        let mut settled = preflight?.clone();
        let Some(preflight_identity) = settled.exact_verification_identity().map(str::to_owned)
        else {
            return Some(settled);
        };
        let stable = verification_call
            .map(|call| self.runtime_execution_attribution(call))
            .and_then(|attribution| attribution.exact_verification_identity().map(str::to_owned))
            .is_some_and(|postflight_identity| postflight_identity == preflight_identity);
        if let Some(metadata) = output.metadata.as_object_mut() {
            metadata.insert("verification_identity_stable".into(), stable.into());
        }
        if stable {
            return Some(settled);
        }
        settled.host.verification_reuse = agent_contracts::VerificationReuse::TaskScoped;
        settled.host.verification_identity.clear();
        if let Some(metadata) = output.metadata.as_object_mut() {
            metadata.insert(
                "verification_reuse".into(),
                "task_scoped_identity_changed".into(),
            );
        }
        Some(settled)
    }

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
                self.record_action_result(
                    &output,
                    ToolResultDisposition::PersistObservation,
                    ActionDispatch::Refused,
                    None,
                );
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result(output.clone(), None);
                }
                let frontier = self.observe_persistable_tool(
                    &output,
                    ToolResultDisposition::PersistObservation,
                    "",
                    None,
                );
                self.report_frontier(frontier).await;
            }
            self.spawn_next_model_or_end(op_tx).await;
            return;
        }
        let mut call = call;
        let attribution = 'preflight: loop {
            let attribution = self.runtime_execution_attribution(&call);
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
                self.record_action_result(
                    &output,
                    ToolResultDisposition::TransientNoPersist,
                    ActionDispatch::Refused,
                    None,
                );
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result_with(
                        output,
                        None,
                        ToolResultDisposition::TransientNoPersist,
                    );
                    if let Some(next) = turn.pending_tools.pop_front() {
                        call = next;
                        continue 'preflight;
                    }
                }
                self.spawn_next_model_or_end(op_tx).await;
                return;
            }
            // A trusted speculative path miss may suppress only an
            // equivalent read/search and only after the workspace confirms
            // that the path is still absent. This is deliberately a live
            // identity check rather than a time-to-live heuristic: external
            // file creation immediately makes the call dispatchable again.
            let negative_fact = self.state.turn.as_ref().and_then(|turn| {
                turn.execution
                    .current_negative_fact(&call.name, &attribution)
            });
            if let Some(fact) = negative_fact
                && let Some(workspace) = self.services.artifact_workspace()
            {
                match workspace.revision(&fact.target).await {
                    Ok(None) => {
                        // Reuse is observable or it does not happen. If the
                        // journal append fails, fail open and execute the
                        // tool rather than hiding a skipped operation.
                        let event = RuntimeEvent::ExecutionNegativeFact {
                            kind: agent_contracts::NegativeFactEventKind::Reused,
                            tool_name: fact.tool_name.clone(),
                            target: fact.target.clone(),
                            failure: fact.failure,
                            workspace_revision: fact.workspace_revision,
                        };
                        if self.core.emit_event(event).await.is_ok() {
                            self.refuse_known_absent_call(&call, &fact.target, &attribution)
                                .await;
                            if let Some(next) = self
                                .state
                                .turn
                                .as_mut()
                                .and_then(|turn| turn.pending_tools.pop_front())
                            {
                                call = next;
                                continue 'preflight;
                            }
                            self.spawn_next_model_or_end(op_tx).await;
                            return;
                        }
                    }
                    Ok(Some(_)) => {
                        // The world changed outside an admitted Runtime
                        // mutation. Remove the stale negative fact and let
                        // the original call observe the newly-existing path.
                        let transition = self.state.turn.as_mut().and_then(|turn| {
                            turn.execution
                                .invalidate_negative_fact(&call.name, &fact.target)
                        });
                        self.report_negative_fact(transition).await;
                    }
                    // A failed oracle check cannot prove equivalence. Run the
                    // tool normally and let its own typed result decide.
                    Err(_) => {}
                }
            }
            // Exact verification PASS reuse has a deliberately smaller
            // equivalence domain than verifier source affinity. It requires
            // the same task anchor, user directive, admitted workspace
            // revision, exact argument digest and host-owned recipe/
            // environment identity. A failed audit append fails open to the
            // real verifier so an unobservable skip is impossible.
            let verification_argument_digest =
                ArgumentDigest::from_json(&call.arguments).to_string();
            let current_anchor_revision = self
                .state
                .task_id
                .and_then(|task_id| self.state.tasks.get(task_id))
                .map(|task| task.anchor.revision);
            let verification_pass = current_anchor_revision.and_then(|anchor_revision| {
                self.state.turn.as_ref().and_then(|turn| {
                    turn.execution.current_exact_verification_pass(
                        &call.name,
                        &verification_argument_digest,
                        anchor_revision,
                        &attribution,
                    )
                })
            });
            if let Some(pass) = verification_pass {
                let event = RuntimeEvent::ExecutionVerificationPass {
                    kind: agent_contracts::VerificationPassEventKind::Reused,
                    tool_name: pass.source_tool_name.clone(),
                    argument_digest: pass.argument_digest.clone(),
                    verification_identity: pass.verification_identity.clone(),
                    anchor_revision: pass.anchor_revision,
                    directive_revision: pass.directive_revision,
                    workspace_revision: pass.workspace_revision,
                };
                if self.core.emit_event(event).await.is_ok() {
                    self.reuse_verification_pass(&call, &pass).await;
                    if let Some(next) = self
                        .state
                        .turn
                        .as_mut()
                        .and_then(|turn| turn.pending_tools.pop_front())
                    {
                        call = next;
                        continue 'preflight;
                    }
                    self.spawn_next_model_or_end(op_tx).await;
                    return;
                }
            }
            // MOD-PROG-01: an identical retry of a deterministic edit
            // refusal against unchanged file identities cannot produce a
            // different result — refuse it before admission so no operation
            // is spawned for a provably no-progress round.
            let duplicate_attempt = self
                .state
                .turn
                .as_ref()
                .and_then(|turn| turn.duplicate_edit_attempt(&call));
            if let Some(attempt) = duplicate_attempt {
                let target = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
                    .or_else(|| {
                        call.arguments
                            .get("files")
                            .and_then(|value| value.as_array())
                            .and_then(|files| files.first())
                            .and_then(|file| file.get("path"))
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                self.refuse_duplicate_call(&call, &target, attempt.failure_class)
                    .await;
                if let Some(next) = self
                    .state
                    .turn
                    .as_mut()
                    .and_then(|turn| turn.pending_tools.pop_front())
                {
                    call = next;
                    continue 'preflight;
                }
                self.spawn_next_model_or_end(op_tx).await;
                return;
            }
            // CONV-02：可证等价的启动失败重试（同参数 + 世界版本未推进）
            // 同样无派发拒绝；超时/退出码等非确定失败永不走这里。
            let duplicate_launch = self
                .state
                .turn
                .as_ref()
                .and_then(|turn| turn.duplicate_launch_failure(&call));
            if let Some(attempt) = duplicate_launch {
                self.refuse_duplicate_call(&call, &attempt.argv0, attempt.failure_class)
                    .await;
                if let Some(next) = self
                    .state
                    .turn
                    .as_mut()
                    .and_then(|turn| turn.pending_tools.pop_front())
                {
                    call = next;
                    continue 'preflight;
                }
                self.spawn_next_model_or_end(op_tx).await;
                return;
            }
            break attribution;
        };
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
                self.settle_aborted_turn().await;
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
                self.settle_aborted_turn().await;
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
                self.settle_aborted_turn().await;
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
        let verification_call = attribution
            .exact_verification_identity()
            .is_some()
            .then(|| call.clone());
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
                recovery_required,
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
                    attribution: Some(attribution),
                    verification_call,
                    tool_identity: Some(completion_identity),
                    value_completion_pending: value_completion_pending
                        && recovery_required.is_none(),
                    recovery_required,
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
        self.settle_aborted_turn().await;
    }

    /// Verify a finished operation still belongs to the current turn and
    /// generation. Stale completions (cancelled or superseded) are dropped
    /// and surfaced as a warning; live ones are committed. A prepared side
    /// effect follows the same fence: roll back when stale, commit when
    /// live — the tool's computation already happened, but its side effect
    /// only lands here.
    /// Keep the actor loop's polling future bounded. Operation settlement
    /// includes effect authority, context observation, frontier accounting,
    /// and the full turn commit path; exposing that concrete future to
    /// `RuntimeActor::run` makes its stack grow with every added settlement
    /// branch. The boxed boundary is one cold allocation per completed
    /// model/tool operation and keeps the always-live actor loop compact.
    pub(super) fn on_operation_completed<'a>(
        &'a mut self,
        completion: OperationCompletion,
        op_tx: &'a mpsc::Sender<OperationCompletion>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(self.on_operation_completed_inner(completion, op_tx))
    }

    async fn on_operation_completed_inner(
        &mut self,
        completion: OperationCompletion,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let execution_recovery = completion.recovery_required.clone();
        if let Some(reason) = execution_recovery.clone() {
            self.require_effect_recovery(format!(
                "tool operation {} could not settle prepared-effect cleanup: {reason}",
                completion.operation.operation_id
            ))
            .await;
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(reason),
                })
                .await;
        }
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
                    tracing::warn!(%error, "Core could not confirm stale-effect rollback settlement");
                    self.state.recovery_required = true;
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: crate::output::bound_error_message(format!(
                                "stale tool operation {} prepared-effect cleanup could not be confirmed: {error}",
                                completion.operation.operation_id
                            )),
                        })
                        .await;
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
                let persistable_delta = self
                    .state
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.turn_frame.has_persistable_tool_delta());
                let structurally_empty =
                    agent_contracts::completion_validity(&content, &tool_calls, &usage)
                        == ModelCompletionValidity::StructurallyEmpty
                        && !persistable_delta;
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
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ModelUsed {
                        input_tokens: usage.input_tokens.unwrap_or(0),
                        output_tokens: usage.output_tokens.unwrap_or(0),
                        attempts: usage.attempts.max(1),
                        retries: usage.retries,
                    })
                    .await;
                if structurally_empty {
                    let retries = self
                        .state
                        .turn
                        .as_ref()
                        .map(|turn| turn.structurally_empty_retries)
                        .unwrap_or(MAX_STRUCTURALLY_EMPTY_RETRIES);
                    if retries < MAX_STRUCTURALLY_EMPTY_RETRIES {
                        if let Some(turn) = self.state.turn.as_mut() {
                            turn.structurally_empty_retries =
                                turn.structurally_empty_retries.saturating_add(1);
                        }
                        self.advance_turn(op_tx).await;
                        return;
                    }
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: "provider returned a structurally empty completion (empty content, no tool calls, 0/0 usage); refusing to complete the turn".into(),
                        })
                        .await;
                    self.settle_aborted_turn().await;
                    self.drain_queued_user_input(op_tx).await;
                    return;
                }
                if tool_calls.len() > MAX_MODEL_TOOL_CALLS_PER_ROUND {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.action_batch = Some(TurnActionBatch::refused_before_dispatch(
                            turn.model_round,
                            tool_calls.len(),
                        ));
                    }
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: format!(
                                "provider returned {} tool calls in one model round; hard safety limit is {}",
                                tool_calls.len(),
                                MAX_MODEL_TOOL_CALLS_PER_ROUND
                            ),
                        })
                        .await;
                    self.settle_aborted_turn().await;
                    self.drain_queued_user_input(op_tx).await;
                    return;
                }
                self.emit_input_consumed().await;
                // The successful decision consumed the previous batch's
                // result-delivery leases. Tools selected by this decision
                // renew those leases. Explicitly loaded tools that have not
                // yet been called remain a directive-local cohort; using the
                // exact tool consumes that pending-load root.
                if let Err(error) = self.reconcile_model_decision_leases(&tool_calls).await {
                    self.fail_round_preparation("tool_leases_reconciled_event", error)
                        .await;
                    self.drain_queued_user_input(op_tx).await;
                    return;
                }
                if tool_calls.is_empty() {
                    self.finalize_turn(content).await;
                    self.drain_queued_user_input(op_tx).await;
                } else {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.action_batch =
                            Some(TurnActionBatch::new(turn.model_round, tool_calls.len()));
                        turn.turn_frame.push_tool_calls(tool_calls.clone());
                        turn.pending_tools.extend(tool_calls);
                    }
                    self.advance_turn(op_tx).await;
                    self.drain_queued_user_input(op_tx).await;
                }
            }
            OperationOutcome::ToolOutput(output) => {
                // A dispatcher/preparation cleanup failure reaches Runtime as
                // a value so the current turn can report the truth. Replace
                // that generic error with the same conservative settlement
                // envelope used by commit failures before it enters the turn
                // frame or ToolFinished event.
                let output = match execution_recovery.as_deref() {
                    Some(reason) => execution_cleanup_recovery_output(output, reason),
                    None => output,
                };
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
                        }) => {
                            self.refresh_runtime_fact_markers();
                            self.accrue_checkpoint_debt(
                                crate::checkpoint::CheckpointDebtReason::DurableWorkspaceMutation,
                            );
                            output
                        }
                        EffectCommitDisposition::Receipt(EffectReceipt::NotApplied { error }) => {
                            not_applied_effect_output(output, &error)
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
                            unsettled_effect_output(
                                output,
                                "applied_recovery_required",
                                "effect applied but recovery is required",
                                "at least one change WAS applied, but the effect operation did not complete durably; recovery is required before another mutation",
                                &error,
                                false,
                            )
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
                            unsettled_effect_output(
                                output,
                                "unknown_recovery_required",
                                "effect applied state unknown",
                                "the change may or may not have been applied; do not retry blindly, and recover before another mutation",
                                &error,
                                false,
                            )
                        }
                        EffectCommitDisposition::AuthorityRecordFailed { receipt, error } => {
                            self.require_effect_recovery(format!(
                                "effect authority record failed after receipt {receipt:?}: {error}"
                            ))
                            .await;
                            match receipt {
                                EffectReceipt::NotApplied {
                                    error: effect_error,
                                } => unsettled_effect_output(
                                    output,
                                    "not_applied_cleanup_recovery_required",
                                    "effect was not applied, but recovery is required",
                                    "the change was not applied, but prepared-effect cleanup or its authority terminal could not be confirmed; recovery is required before another mutation",
                                    &format!("{effect_error}; settlement error: {error}"),
                                    true,
                                ),
                                EffectReceipt::Applied { .. } => unsettled_effect_output(
                                    output,
                                    "applied_authority_recovery_required",
                                    "effect applied but authority recovery is required",
                                    "the change WAS applied, but Core could not record the terminal operation state; recovery is required before another mutation",
                                    &error,
                                    false,
                                ),
                                EffectReceipt::Unknown {
                                    error: effect_error,
                                } => unsettled_effect_output(
                                    output,
                                    "unknown_authority_recovery_required",
                                    "effect state unknown",
                                    "the change may or may not have been applied, and Core could not record the terminal operation state; do not retry blindly, and recover before another mutation",
                                    &format!("{effect_error}; authority record error: {error}"),
                                    false,
                                ),
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
                                EffectCommitRejection::ActualExceedsApproved => {
                                    "the prepared effect reported a workspace write to a path the approved intent never named"
                                }
                                EffectCommitRejection::BrokerUnavailable => {
                                    "the effect broker could not reserve the approved effect before dispatch; nothing was applied"
                                }
                                EffectCommitRejection::BindingRevoked => {
                                    "the tool's admitted authority binding was revoked or replaced after this lease was issued; nothing was applied"
                                }
                            };
                            unsettled_effect_output(
                                output,
                                "rejected",
                                "effect authorization rejected before commit",
                                "the change was not applied because Core rejected its commit authority",
                                detail,
                                true,
                            )
                        }
                    },
                    None => output,
                };
                // Last-line invariant guard: untrusted capability/process
                // outputs and context fetches must never enter the turn
                // frame, context engine or event stream unbounded. Normal
                // tools spill before this point; this guard makes a
                // producer contract violation safe and visible.
                let mut output = bound_tool_output(output);
                let settled_attribution = self.settled_execution_attribution(
                    completion.attribution.as_ref(),
                    completion.verification_call.as_ref(),
                    &mut output,
                );
                self.stamp_fs_read_motive(&mut output).await;
                // Terminalize the accepted value while the operation's
                // admission generation is still current: an applied runtime
                // directive below may advance the authority epoch (an
                // accepted `task.manage` anchor CAS does), and recording
                // afterwards would make this very operation stale and
                // raise a false recovery fence.
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
                // Execute the tool's runtime directive now, as part of the
                // operation commit — not at turn end — so a context control
                // request takes effect before the next model round. A task
                // progress proposal additionally writes its CAS outcome back
                // into the model-visible result.
                if let Some(directive) = completion.directive {
                    match directive {
                        RuntimeDirective::UpdateTaskProgress(proposal) => {
                            self.apply_task_progress_proposal(&mut output, proposal)
                                .await;
                        }
                        other => self.execute_directive(other).await,
                    }
                }
                if output.ok
                    && matches!(
                        output.tool_name.as_str(),
                        "shell.exec" | "process.run" | "verify.run"
                    )
                {
                    self.refresh_runtime_fact_markers();
                    if output.tool_name == "verify.run" {
                        self.accrue_checkpoint_debt(
                            crate::checkpoint::CheckpointDebtReason::VerificationChanged,
                        );
                    }
                }
                // Successful semantic observations heat related context
                // before the next model round. Failed execution results
                // stay on the TurnFrame only.
                if output.heats_working_set() {
                    let _ = self
                        .services
                        .context_ingest(ContextIngress::WorkingSetSignal {
                            resources: output.resource_touches(),
                            content: String::new(),
                        })
                        .await;
                }
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result_with(
                        output.clone(),
                        op_scope_id,
                        completion.disposition,
                    );
                    // Exactly-once close scheduling: this result's frame is
                    // consumed by the next model request, so its context
                    // scope closes at that boundary — never scanned again.
                    if let Some(scope_id) = op_scope_id {
                        turn.pending_scope_closes.push_back(scope_id);
                    }
                }
                self.record_action_result(
                    &output,
                    completion.disposition,
                    ActionDispatch::Spawned,
                    settled_attribution
                        .as_ref()
                        .map(RuntimeExecutionAttribution::reusable_verification),
                );
                self.update_result_delivery_from_catalog_control(&output);
                let frontier = self.observe_persistable_tool(
                    &output,
                    completion.disposition,
                    completion
                        .argument_digest
                        .as_ref()
                        .map(|digest| digest.to_string())
                        .unwrap_or_default()
                        .as_str(),
                    settled_attribution.as_ref(),
                );
                // MOD-PROG-01: remember deterministic edit refusals so an
                // identical retry can be refused without dispatch.
                if let Some(digest) = completion.argument_digest
                    && let Some(turn) = self.state.turn.as_mut()
                {
                    turn.record_edit_attempt(&output, &digest);
                    // CONV-02：程序解析失败同样入账（可证等价域才收）。
                    turn.record_launch_failure(&output, &digest);
                }
                // PROTO-EVID-01：正文入当轮缓存（含失效规则）。
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.record_protocol_body(&output);
                }
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::ToolFinished { output })
                    .await;
                self.report_frontier(frontier).await;
                // LONG-TASK SAFE POINT: the batch is terminally settled and
                // nothing is in flight; accrued debt now installs the
                // bounded resume and schedules one atomic write before the
                // next model decision.
                self.safe_point_resume_commit().await;
                // Advisory consult on the freshly observed batch (default
                // off): spends a spent lease, may offer a fresh one.
                self.settle_completion_opportunity().await;
                if let Some(summary) = self.terminal_completion_summary() {
                    self.finalize_terminal_completion(summary).await;
                } else if let Some(reason) = self.completion_gate_refusal() {
                    // The proposal exists but the acceptance gate refuses:
                    // surface it once per turn and return the decision to
                    // the model instead of committing.
                    let turn_id = self.state.turn.as_ref().map(|turn| turn.turn_id);
                    if self.state.completion_refusal_surfaced_for != turn_id {
                        let _ = self
                            .core
                            .emit_warning(format!("completion gate refused: {reason}"))
                            .await;
                        self.state.completion_refusal_surfaced_for = turn_id;
                        self.refuse_completion_opportunity(reason).await;
                    }
                    self.advance_turn(op_tx).await;
                } else {
                    self.advance_turn(op_tx).await;
                }
                self.drain_queued_user_input(op_tx).await;
            }
            OperationOutcome::Failed { message } => {
                let _ = self.core.emit_event(RuntimeEvent::Error { message }).await;
                // Provider failure, not runtime corruption: settle the
                // applied input and drop the turn without fencing.
                self.settle_aborted_turn().await;
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

    /// Classify this `fs.read` against engine residency + resource facts
    /// and stamp the E2E motive onto the output before it is journaled.
    async fn stamp_fs_read_motive(&self, output: &mut ToolOutput) {
        if output.tool_name != "fs.read" {
            return;
        }
        let Some(touch) = output.resource_touches().into_iter().next() else {
            return;
        };
        if touch.path.is_empty() {
            return;
        }
        let residency = self
            .services
            .context_fs_read_residency(&touch.path)
            .await
            .unwrap_or(FsRereadClass::FirstRead);
        let prior = self.projected_resource_fact(&touch.path);
        let motive = crate::execution::classify_fs_read_motive(
            residency,
            prior.as_ref(),
            touch.revision.as_deref(),
        );
        crate::execution::stamp_fs_read_motive(output, motive);
    }

    fn projected_resource_fact(&self, path: &str) -> Option<crate::execution::ResourceFact> {
        if let Some(turn) = self.state.turn.as_ref() {
            return turn.execution.fact_for(path).cloned();
        }
        let task_id = self.state.task_id?;
        let task = self.state.tasks.get(task_id)?;
        task.resume.fact_for(path).cloned()
    }

    fn observe_persistable_tool(
        &mut self,
        output: &ToolOutput,
        disposition: ToolResultDisposition,
        argument_digest: &str,
        attribution: Option<&RuntimeExecutionAttribution>,
    ) -> Option<crate::execution::FrontierObservation> {
        if disposition != ToolResultDisposition::PersistObservation {
            return None;
        }
        let task_id = self.state.task_id?;
        let task = self.state.tasks.get(task_id)?;
        if task.status == crate::task::TaskStatus::Completed {
            return None;
        }
        let anchor_revision = task.anchor.revision;
        let turn = self.state.turn.as_mut()?;
        let turn_number = turn.model_round as u64;
        let observation = match attribution {
            Some(attribution) => turn.execution.observe_tool_attributed(
                output,
                anchor_revision,
                turn_number,
                argument_digest,
                attribution,
            ),
            None => turn.execution.observe_tool_with_digest(
                output,
                anchor_revision,
                turn_number,
                argument_digest,
            ),
        };
        Some(observation)
    }

    /// A successful catalog load happens after the model decision that
    /// requested it, so the target was not yet present in that decision's
    /// exact call roots. Keep it pending until the model calls that exact
    /// tool, explicitly unloads it, or ends the directive. This source-driven
    /// lifetime lets sequential loads form a usable cohort without a fixed
    /// round TTL. The unified control tool is runtime-owned and cannot be
    /// shadowed, making this metadata a trusted lifecycle receipt.
    fn update_result_delivery_from_catalog_control(&mut self, output: &ToolOutput) {
        if !output.ok || output.tool_name != CAPABILITY_MANAGE {
            return;
        }
        let Some(op) = output.metadata.get("op").and_then(|value| value.as_str()) else {
            return;
        };
        let Some(tool_name) = output
            .metadata
            .get("tool")
            .and_then(|value| value.as_str())
            .filter(|name| !name.is_empty())
        else {
            return;
        };
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        match op {
            "load" => {
                if !turn
                    .pending_loaded_tools
                    .iter()
                    .any(|name| name == tool_name)
                {
                    turn.pending_loaded_tools.push(tool_name.to_string());
                    turn.pending_loaded_tools.sort();
                }
            }
            "unload" => {
                turn.pending_loaded_tools.retain(|name| name != tool_name);
                turn.result_delivery_tools.retain(|name| name != tool_name);
            }
            _ => {}
        }
    }

    fn record_action_result(
        &mut self,
        output: &ToolOutput,
        disposition: ToolResultDisposition,
        dispatch: ActionDispatch,
        trusted_verification: Option<bool>,
    ) {
        let facts = self.services.tools().execution_facts(output);
        if let Some(batch) = self
            .state
            .turn
            .as_mut()
            .and_then(|turn| turn.action_batch.as_mut())
        {
            batch.record(output, disposition, dispatch, trusted_verification, &facts);
        }
    }

    /// Settle the current body-free action ledger before the next model
    /// request or a terminal interruption. It intentionally runs even when
    /// accounting has a gap, so cancellation/error paths remain visible
    /// instead of silently dropping the incomplete batch.
    pub(super) async fn settle_action_batch(&mut self) -> AgentResult<()> {
        let Some((turn_id, batch)) = self
            .state
            .turn
            .as_mut()
            .and_then(|turn| turn.action_batch.take().map(|batch| (turn.turn_id, batch)))
        else {
            return Ok(());
        };
        let missing_terminal = batch.requested.saturating_sub(batch.terminal);
        let unexpected_terminal = batch.terminal.saturating_sub(batch.requested);
        self.core
            .emit_event(RuntimeEvent::ExecutionBatchSettled {
                turn_id,
                model_round: batch.model_round,
                requested: batch.requested,
                terminal: batch.terminal,
                spawned: batch.spawned,
                refused: batch.refused,
                reused: batch.reused,
                persist_observation: batch.persist_observation,
                transient_no_persist: batch.transient_no_persist,
                access_event_only: batch.access_event_only,
                succeeded: batch.succeeded,
                failed: batch.failed,
                known_mutation_results: batch.known_mutation_results,
                typed_verification_results: batch.typed_verification_results,
                unknown_invalidations: batch.unknown_invalidations,
                completion_proposals: batch.completion_proposals,
                outcome_advances: batch.outcome_advances,
                no_outcome_results: batch.no_outcome_results,
                missing_terminal,
                unexpected_terminal,
            })
            .await
    }

    /// 把一轮前沿分类作为 `ExecutionFrontier` 事件上报。事件是有界
    /// 计数，不含任何工具正文；收敛指标从这里聚合。同批的义务账目
    /// 事件（CONV-OBS-01）逐条出账，报告可从事件流独立验证
    /// max_attempts_per_epoch / max_total_attempts_per_lineage。
    async fn report_frontier(
        &mut self,
        observation: Option<crate::execution::FrontierObservation>,
    ) {
        let Some(observation) = observation else {
            return;
        };
        for event in observation.obligation_events {
            let _ = self
                .core
                .emit_event(RuntimeEvent::ExecutionObligation {
                    kind: event.kind,
                    domain: event.domain,
                    scope_digest: event.scope_digest,
                    epoch: event.epoch,
                    attempts_in_epoch: event.attempts_in_epoch,
                    total_attempts: event.total_attempts,
                })
                .await;
        }
        for event in observation.negative_fact_events {
            self.report_negative_fact(Some(event)).await;
        }
        for event in observation.verification_pass_events {
            let _ = self
                .core
                .emit_event(RuntimeEvent::ExecutionVerificationPass {
                    kind: event.kind,
                    tool_name: event.tool_name,
                    argument_digest: event.argument_digest,
                    verification_identity: event.verification_identity,
                    anchor_revision: event.anchor_revision,
                    directive_revision: event.directive_revision,
                    workspace_revision: event.workspace_revision,
                })
                .await;
        }
        let _ = self
            .core
            .emit_event(RuntimeEvent::ExecutionFrontier {
                delta: observation.delta,
                actions_since_frontier_advance: observation.actions_since_frontier_advance,
                evidence_revision: observation.evidence_revision,
                invalidated: observation.invalidated,
            })
            .await;
    }

    async fn report_negative_fact(
        &self,
        transition: Option<crate::execution::NegativeFactTransition>,
    ) {
        let Some(transition) = transition else {
            return;
        };
        let _ = self
            .core
            .emit_event(RuntimeEvent::ExecutionNegativeFact {
                kind: transition.kind,
                tool_name: transition.tool_name,
                target: transition.target,
                failure: transition.failure,
                workspace_revision: transition.workspace_revision,
            })
            .await;
    }

    /// 无派发拒绝一次可证等价的重试：类型化 ToolFinished 入账、推进
    /// turn frame、观察与前沿上报，然后结束本轮。
    async fn refuse_duplicate_call(
        &mut self,
        call: &ToolCall,
        target: &str,
        failure_class: agent_contracts::ToolFailureClass,
    ) {
        let output = duplicate_no_progress_output(&call.id, &call.name, target, failure_class);
        self.finish_reused_call(output, "", None).await;
    }

    async fn refuse_known_absent_call(
        &mut self,
        call: &ToolCall,
        target: &str,
        attribution: &RuntimeExecutionAttribution,
    ) {
        let output = known_absent_reuse_output(call, target);
        let argument_digest = ArgumentDigest::from_json(&call.arguments).to_string();
        self.finish_reused_call(output, &argument_digest, Some(attribution))
            .await;
    }

    async fn reuse_verification_pass(
        &mut self,
        call: &ToolCall,
        pass: &crate::execution::VerificationFact,
    ) {
        let output = exact_verification_reuse_output(call, pass);
        let _ = self
            .core
            .emit_event(RuntimeEvent::ToolFinished {
                output: output.clone(),
            })
            .await;
        self.record_action_result(
            &output,
            ToolResultDisposition::PersistObservation,
            ActionDispatch::Reused,
            Some(true),
        );
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_frame.push_tool_result(output.clone(), None);
        }
        let frontier = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.anchor.revision)
            .and_then(|anchor_revision| {
                self.state.turn.as_mut().map(|turn| {
                    turn.execution.observe_reused_verification(
                        &output,
                        anchor_revision,
                        turn.model_round as u64,
                    )
                })
            });
        self.report_frontier(frontier).await;
    }

    async fn finish_reused_call(
        &mut self,
        output: ToolOutput,
        argument_digest: &str,
        attribution: Option<&RuntimeExecutionAttribution>,
    ) {
        let _ = self
            .core
            .emit_event(RuntimeEvent::ToolFinished {
                output: output.clone(),
            })
            .await;
        self.record_action_result(
            &output,
            ToolResultDisposition::PersistObservation,
            ActionDispatch::Reused,
            attribution.map(RuntimeExecutionAttribution::reusable_verification),
        );
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_frame.push_tool_result(output.clone(), None);
        }
        let frontier = self.observe_persistable_tool(
            &output,
            ToolResultDisposition::PersistObservation,
            argument_digest,
            attribution,
        );
        self.report_frontier(frontier).await;
    }

    /// Poison the normal-mutation lane after an effect result proves that
    /// the world cannot safely be used as the base for more work. The flag
    /// is set before best-effort observability writes, so a failed warning
    /// or event append cannot accidentally leave mutation enabled. The
    /// active turn is deliberately not aborted: it must carry the truthful
    /// receipt back to the model/user and durably record that outcome.
    pub(super) async fn require_effect_recovery(&mut self, warning: String) {
        let warning = crate::output::bound_error_message(warning);
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

/// MOD-PROG-01: the deterministic duplicate-refusal result. Nothing was
/// executed, so this is `NotApplied` by construction; the message names
/// the original failure so the model can change strategy instead of
/// resubmitting the same arguments.
fn duplicate_no_progress_output(
    call_id: &str,
    tool_name: &str,
    target: &str,
    original_failure: agent_contracts::ToolFailureClass,
) -> ToolOutput {
    let target_line = if target.is_empty() {
        String::new()
    } else {
        format!("\ntarget={target}")
    };
    ToolOutput {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        ok: false,
        summary: "duplicate_no_progress: identical edit retry refused without dispatch".into(),
        model_content: format!(
            "duplicate_no_progress: this exact call already failed with {} and the target files are unchanged, so the result would be identical. Change the arguments, re-read the target, or finish with the current state.{target_line}",
            original_failure.as_str(),
        ),
        artifact_ref: None,
        metadata: serde_json::json!({
            "path": target,
            "failure_class": agent_contracts::ToolFailureClass::DuplicateNoProgress.as_str(),
            "original_failure_class": original_failure.as_str(),
            "executed": false,
        }),
    }
}

/// Truthful no-dispatch projection for a revision-bound negative fact. The
/// original `path_not_found` remains the failure identity; `executed=false`
/// distinguishes reuse from a second filesystem call.
fn known_absent_reuse_output(call: &ToolCall, target: &str) -> ToolOutput {
    ToolOutput {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        ok: false,
        summary: "known_absent: current workspace still has no such path".into(),
        model_content: format!(
            "known_absent: {target} was already observed missing and a live workspace check confirms it is still absent. No tool was executed; use a rooted task path, inspect its parent, change strategy, or finish with the current evidence."
        ),
        artifact_ref: None,
        metadata: serde_json::json!({
            "path": target,
            "failure_class": agent_contracts::ToolFailureClass::PathNotFound.as_str(),
            "executed": false,
            "negative_fact_reused": true,
        }),
    }
}

/// Truthful no-dispatch projection of an exact, current verification PASS.
/// The original bounded summary/artifact remain the evidence; Runtime adds
/// only the fact that the complete equivalence tuple was unchanged.
fn exact_verification_reuse_output(
    call: &ToolCall,
    pass: &crate::execution::VerificationFact,
) -> ToolOutput {
    ToolOutput {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        ok: true,
        summary: pass.summary.clone(),
        model_content: format!(
            "verification_pass_reused: the trusted verifier already passed under the same task directive, arguments, workspace revision, recipe and environment identity. No process was executed. Prior result: {}",
            pass.summary
        ),
        artifact_ref: pass.evidence_ref.clone(),
        metadata: serde_json::json!({
            "verification": true,
            "verification_pass_reused": true,
            "executed": false,
            "anchor_revision": pass.anchor_revision,
            "directive_revision": pass.directive_revision,
            "workspace_revision": pass.workspace_revision,
        }),
    }
}

#[cfg(test)]
mod settlement_output_tests {
    use super::*;
    use agent_contracts::ToolFailureClass;

    #[test]
    fn commit_conflict_drops_proposed_revisions_and_is_typed() {
        let output = ToolOutput {
            call_id: "c".into(),
            tool_name: "edit.patch".into(),
            ok: true,
            summary: "prepared".into(),
            model_content: "proposed revision deadbeef".into(),
            artifact_ref: None,
            metadata: serde_json::json!({
                "changed": true,
                "files": [{"path": "src/lib.rs", "revision": "deadbeef"}],
            }),
        };
        let failed = not_applied_effect_output(
            output,
            "stale_revision: target changed after mutation preflight",
        );
        assert!(!failed.ok);
        assert_eq!(
            failed.failure_class(),
            Some(ToolFailureClass::StaleRevision)
        );
        assert_eq!(failed.metadata["commit_state"], "not_applied");
        assert_eq!(failed.metadata["attempted_paths"][0], "src/lib.rs");
        assert!(failed.metadata.get("files").is_none());
        assert!(failed.resource_touches().is_empty());
        assert!(!failed.model_content.contains("deadbeef"));
    }

    #[test]
    fn execution_cleanup_failure_is_typed_and_drops_proposed_revisions() {
        let output = ToolOutput {
            call_id: "c".into(),
            tool_name: "edit.patch".into(),
            ok: false,
            summary: "generic recovery error with proposed revision deadbeef".into(),
            model_content: "generic recovery error with proposed revision deadbeef".into(),
            artifact_ref: Some("artifact://proposed".into()),
            metadata: serde_json::json!({
                "path": "src/lib.rs",
                "revision": "deadbeef",
                "files": [{"path": "src/other.rs", "revision": "cafebabe"}],
            }),
        };

        let failed =
            execution_cleanup_recovery_output(output, &format!("START{}END", "x".repeat(10_000)));

        assert!(!failed.ok);
        assert_eq!(
            failed.metadata["commit_state"],
            "execution_cleanup_recovery_required"
        );
        assert_eq!(
            failed.metadata["attempted_paths"],
            serde_json::json!(["src/lib.rs", "src/other.rs"])
        );
        assert!(failed.metadata.get("files").is_none());
        assert!(failed.metadata.get("revision").is_none());
        assert!(failed.resource_touches().is_empty());
        assert!(failed.artifact_ref.is_none());
        assert!(!failed.summary.contains("deadbeef"));
        assert!(!failed.model_content.contains("deadbeef"));
        assert!(failed.summary.chars().count() <= agent_contracts::MAX_TOOL_SUMMARY_CHARS);
        assert!(
            serde_json::to_vec(&failed.metadata).unwrap().len()
                <= agent_contracts::MAX_TOOL_METADATA_BYTES
        );
    }

    #[test]
    fn every_unsettled_projection_is_bounded_and_drops_proposed_facts() {
        let files: Vec<_> = (0..agent_contracts::MAX_RESOURCE_TOUCHES + 4)
            .map(|index| {
                serde_json::json!({
                    "path": format!("src/{index}/{}.rs", "😀".repeat(400)),
                    "revision": "deadbeef",
                })
            })
            .collect();
        let output = ToolOutput {
            call_id: "c".into(),
            tool_name: "edit.patch".into(),
            ok: true,
            summary: "prepared deadbeef".into(),
            model_content: "proposed revision deadbeef".into(),
            artifact_ref: Some("artifact://proposed".into()),
            metadata: serde_json::json!({"files": files}),
        };

        for (state, diagnose) in [
            ("applied_recovery_required", false),
            ("unknown_recovery_required", false),
            ("not_applied_cleanup_recovery_required", true),
            ("applied_authority_recovery_required", false),
            ("unknown_authority_recovery_required", false),
            ("execution_cleanup_recovery_required", true),
            ("rejected", true),
        ] {
            let failed = unsettled_effect_output(
                output.clone(),
                state,
                "settlement",
                "world state",
                &format!("START{}END", "x".repeat(10_000)),
                diagnose,
            );
            assert!(!failed.ok);
            assert_eq!(failed.metadata["commit_state"], state);
            assert!(failed.metadata.get("files").is_none());
            assert!(failed.resource_touches().is_empty());
            assert!(failed.artifact_ref.is_none());
            assert!(!failed.summary.contains("deadbeef"));
            assert!(!failed.model_content.contains("deadbeef"));
            assert!(failed.summary.chars().count() <= agent_contracts::MAX_TOOL_SUMMARY_CHARS);
            let attempted = failed.metadata["attempted_paths"].as_array().unwrap();
            assert!(attempted.len() <= agent_contracts::MAX_RESOURCE_TOUCHES);
            assert!(
                attempted.len() < agent_contracts::MAX_RESOURCE_TOUCHES,
                "four-byte paths must be dropped until serialized metadata fits"
            );
            assert!(attempted.iter().all(|path| {
                path.as_str().unwrap().chars().count() <= agent_contracts::MAX_RESOURCE_PATH_CHARS
            }));
            assert!(
                serde_json::to_vec(&failed.metadata).unwrap().len()
                    <= agent_contracts::MAX_TOOL_METADATA_BYTES
            );
        }
    }
}

use super::*;

impl RuntimeActor {
    /// Prepare + spawn one model round: close the consumed tool frames,
    /// maintenance, materialize, assemble, then the model call as an
    /// operation.
    pub(super) async fn spawn_model_operation(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        // The previous round's tool frames end here: the model request below
        // consumes their results (they ride in the turn frame).
        if let Err(error) = self.close_tool_frames().await {
            // Ordinary round cleanup is best-effort and observable. The
            // model can continue from the bounded turn frame; cancellation
            // uses the strict path below and refuses to acknowledge success.
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(error.to_string()),
                })
                .await;
        }

        // Copy the immutable round inputs out of ActorState before awaiting.
        // The actor is serialized, but short borrows also make it impossible
        // to accidentally publish a partially packed surface into ActiveTurn.
        let (turn_id, model_round, current_input, turn_frame) = {
            let Some(turn) = self.state.turn.as_mut() else {
                return;
            };
            turn.model_round += 1;
            (
                turn.turn_id,
                turn.model_round,
                turn.turn_frame.user_message.clone(),
                turn.turn_frame.clone(),
            )
        };

        match self
            .services
            .context_maintain(ContextMaintenanceTrigger::BeforeModel)
            .await
        {
            Ok(report) => {
                if let Err(error) = self
                    .emit_context_maintained(ContextMaintenanceTrigger::BeforeModel, report)
                    .await
                {
                    // The maintenance state change landed but its audit
                    // event did not: fence the turn instead of letting the
                    // state silently outrun its journal event.
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    self.state.turn = None;
                    return;
                }
            }
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
        }

        // Tool lifecycle safe point. The active task's tool-demand set is
        // the GC root set: a tool the task requires is never aged out by
        // idle GC, so task demand cannot silently evaporate from the
        // surface. Task demand is declarative only: reload can restore
        // catalog/schema readiness, but cannot enable a disabled
        // capability, grant a permission or bypass approval/effect policy.
        let task_roots: Vec<String> = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| {
                task.tool_requirements
                    .entries
                    .iter()
                    .map(|requirement| requirement.tool_name.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.services.tool_gc(&task_roots);
        let active_task = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id));
        let (task_requirement_revision, mut requirements) = active_task
            .map(|task| {
                (
                    Some(task.tool_requirements.revision),
                    task.tool_requirements.entries.clone(),
                )
            })
            .unwrap_or((None, Vec::new()));

        // Reload only requirements that GC actually moved off-surface. The
        // final snapshot below is authoritative, so a refused load is
        // represented as Unavailable without leaking provider error text.
        let mut visible_names: HashSet<String> = self
            .services
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        visible_names.extend(
            self.services
                .tool_catalog()
                .into_iter()
                .filter(|entry| entry.state.in_surface())
                .map(|entry| entry.name),
        );
        for requirement in &requirements {
            if !visible_names.contains(&requirement.tool_name) {
                let _ = self.services.tool_load(&requirement.tool_name);
            }
        }

        // Dispatcher snapshot is the complete currently-loaded candidate
        // set. Runtime owns the sole bounded projection so Task MustSurface
        // can never disappear inside a provider adapter before policy sees it.
        let candidates = self.services.tool_snapshot();
        let candidate_names: HashSet<String> = candidates
            .specs
            .iter()
            .map(|spec| spec.name.clone())
            .collect();

        // Derive typed tool roots from the task anchor / focus goal and the
        // active-call policy, then merge them into the explicit requirement
        // set. Derivation is a pure function of the safe-point state and
        // only names tools that exist in the candidate catalog; the explicit
        // task-owned set stays the authority (higher demand ranks win).
        let anchor = active_task.map(|task| &task.anchor);
        let active_tool = self.state.active_tool.as_deref();
        requirements.extend(crate::policy::derive_task_roots(
            crate::policy::TaskRootInput {
                anchor,
                focus_goal: active_task.map(|task| task.goal.as_str()),
                active_tool,
                catalog_names: &candidate_names,
            },
        ));

        let mut unavailable_must = Vec::new();
        let mut unavailable_optional = Vec::new();
        for requirement in &requirements {
            if !candidate_names.contains(requirement.tool_name.as_str()) {
                if requirement.demand == ToolSurfaceDemand::MustSurface {
                    unavailable_must.push(ToolSurfaceBlock {
                        tool_name: requirement.tool_name.clone(),
                        demand: requirement.demand,
                        reason: ToolSurfaceBlockReason::Unavailable,
                    });
                } else {
                    unavailable_optional.push(requirement.clone());
                }
            }
        }

        let mut surface_plan = RoundSurfacePlan::build(candidates, &requirements, |name| {
            self.services.tool_may_omit_from_round(name)
        });
        surface_plan
            .source_revisions_mut()
            .task_requirement_revision = task_requirement_revision;
        surface_plan.source_revisions_mut().anchor_revision = anchor.map(|a| a.revision);
        surface_plan.source_revisions_mut().focus_revision =
            self.state.task_id.map(|_| self.state.focus_revision);
        surface_plan
            .source_revisions_mut()
            .execution_policy_revision =
            crate::policy::derive_execution_policy_revision(active_tool);
        for requirement in &unavailable_optional {
            surface_plan.add_unavailable(requirement);
        }

        if !unavailable_must.is_empty() {
            let surface_revision = match self.issue_surface_revision() {
                Ok(revision) => revision,
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
            let report = surface_plan.unsatisfiable_report(
                SurfaceReportContext {
                    turn_id,
                    model_round,
                    surface_revision,
                    estimated_input_tokens: 0,
                    input_budget_tokens: 0,
                },
                ToolSurfaceBlockReason::Unavailable,
                unavailable_must,
            );
            if let Err(error) = self
                .core
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!(
                            "failed to persist the unavailable-tool surface decision ({error}); refusing to start the model round"
                        ),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: "the active task requires a tool that is unavailable; refusing to start the model round"
                        .into(),
                })
                .await;
            self.state.turn = None;
            return;
        }

        if surface_plan.mandatory_schema_tokens() > MAX_TOOL_SURFACE_TOKENS {
            let surface_revision = match self.issue_surface_revision() {
                Ok(revision) => revision,
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
            let blocked = surface_plan.mandatory_blocks(ToolSurfaceBlockReason::SchemaBudget);
            let report = surface_plan.unsatisfiable_report(
                SurfaceReportContext {
                    turn_id,
                    model_round,
                    surface_revision,
                    estimated_input_tokens: surface_plan.mandatory_schema_tokens(),
                    input_budget_tokens: MAX_TOOL_SURFACE_TOKENS,
                },
                ToolSurfaceBlockReason::SchemaBudget,
                blocked,
            );
            if let Err(error) = self
                .core
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!(
                            "failed to persist the schema-budget surface decision ({error}); refusing to start the model round"
                        ),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!(
                        "mandatory tool schemas exceed the per-round schema budget ({} > {} tokens); refusing to start the model round",
                        surface_plan.mandatory_schema_tokens(),
                        MAX_TOOL_SURFACE_TOKENS
                    ),
                })
                .await;
            self.state.turn = None;
            return;
        }

        // 发送窗口与打包窗口分离：SWE-bench 工具轮的 turn frame 必须
        // 能发出去；C 的 working set 仍按内核 pack cap 收。未声明
        // provider 窗口时两者都回退到内核 budget（旧行为）。
        let capabilities = self.services.model_capabilities();
        let turn_frame_tokens = approx_layer_tokens(&turn_frame.messages());
        let active_tools_tokens = approx_layer_tokens(&surface_plan.specs());
        let kernel_budget = self.services.context_budget_tokens();
        let send_window = provider_send_window(capabilities.context_window, kernel_budget);
        let pack_window = engine_pack_window(capabilities.context_window, kernel_budget);
        // The output reserve is a hard subtraction: the answer must always
        // have room, and rendering overhead must never eat into it.
        let output_reserve = if capabilities.max_output_tokens > 0 {
            capabilities.max_output_tokens
        } else {
            DEFAULT_OUTPUT_RESERVE
        };
        let (runtime_focus, task_view, progress_view) = self.runtime_prompt_focus(&turn_frame);
        let runtime_focus_frame_tokens = crate::prompt::focus_frame_tokens(
            runtime_focus.as_ref(),
            task_view.as_ref(),
            progress_view.as_ref(),
        );
        let model_budget = ModelBudget::compute(
            pack_window,
            output_reserve,
            self.assembler.system_prompt_tokens(),
            runtime_focus_frame_tokens,
            turn_frame_tokens,
            active_tools_tokens,
        );
        let materialize_started = std::time::Instant::now();
        // 当前活跃任务锚的根声明投影：PromptRequired 的声明会强制条目进帧。
        // TaskAnchorView 和 Focus 由 PromptAssembler 从 TaskManager 取，
        // 不再经引擎 materialize 回传。hints.task 仍投影给引擎内部使用。
        let anchor_roots = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| crate::task::anchor_root_claims(&task.anchor))
            .unwrap_or_default();
        let materialized = match self
            .services
            .context_materialize(ContextQuery {
                current_input: current_input.clone(),
                budget_tokens: model_budget.context_frame_budget,
                hints: ContextHints {
                    max_selected_items: Some(CONTEXT_CONSUMPTION_ACK_ITEM_CAP),
                    anchor_roots,
                    task: task_view.clone(),
                    checked_files: progress_view
                        .as_ref()
                        .map(|view| view.checked_files.clone())
                        .unwrap_or_default(),
                },
            })
            .await
        {
            Ok(materialized) => materialized,
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
        let materialize_ms = materialize_started.elapsed().as_millis() as u64;
        // Runtime final guard: the engine priced the working-set content,
        // but the assembler's rendering overhead (section headers, per-item
        // frame labels) is the runtime's share. The assembled request must
        // fit the *send* input budget — the provider window minus the
        // output reserve — because the answer must always have room. Trim
        // the context frame until it fits; if the fixed layers alone
        // (system + turn + tools) still overshoot, omit optional schemas
        // from this round snapshot; a request whose mandatory fixed layers
        // still do not fit is a hard error, never a lifecycle mutation or
        // silently over-budget send.
        let max_input_budget = send_window.saturating_sub(output_reserve);
        let mut materialized = materialized;
        let mut input =
            self.assemble_model_input(&materialized, &turn_frame, surface_plan.specs().to_vec());
        let assembled_total = |input: &ModelInput| {
            approx_layer_tokens(&input.into_messages()) + approx_layer_tokens(&input.tool_schemas)
        };
        while assembled_total(&input) > max_input_budget && !materialized.items.is_empty() {
            // Drop the largest unpinned item first (pinned items keep
            // priority); when only pinned items remain, drop the largest
            // anyway rather than overshoot the input budget.
            let drop_index = materialized
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.retention != ContextRetention::Pinned)
                .max_by_key(|(_, item)| approx_tokens(&item.content))
                .or_else(|| {
                    materialized
                        .items
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, item)| approx_tokens(&item.content))
                })
                .map(|(index, _)| index);
            let Some(drop_index) = drop_index else {
                break;
            };
            let dropped = materialized.items.remove(drop_index);
            materialized
                .selected
                .retain(|selection| selection.item_id != dropped.item_id);
            materialized.approx_tokens = materialized
                .approx_tokens
                .saturating_sub(approx_tokens(&dropped.content));
            input = self.assemble_model_input(
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
        }

        // The context frame is empty but the fixed layers still overshoot:
        // omit optional schemas from this round's snapshot only. Provider
        // token pressure must never unload a catalog entry, bump its
        // generation or make a later, larger-budget round forget the tool.
        // The trimmed snapshot remains the one source for prompt assembly,
        // accounting and tool-call validation in this round.
        while assembled_total(&input) > max_input_budget {
            if surface_plan.omit_largest_for_provider_budget().is_none() {
                break;
            }
            input = self.assemble_model_input(
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
        }

        let estimated_input_tokens = assembled_total(&input);
        let surface_revision = match self.issue_surface_revision() {
            Ok(revision) => revision,
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

        // ContextPrepared now describes the final packed frame, not the
        // engine's larger preview before runtime rendering overhead was paid.
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ContextPrepared {
                diagnostics: materialized.diagnostics.clone(),
                selected: materialized.selected.clone(),
                materialize_ms,
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
            self.state.turn = None;
            return;
        }

        if estimated_input_tokens > max_input_budget {
            let blocked =
                surface_plan.mandatory_blocks(ToolSurfaceBlockReason::ProviderInputBudget);
            let report = surface_plan.unsatisfiable_report(
                SurfaceReportContext {
                    turn_id,
                    model_round,
                    surface_revision,
                    estimated_input_tokens,
                    input_budget_tokens: max_input_budget,
                },
                ToolSurfaceBlockReason::ProviderInputBudget,
                blocked,
            );
            if let Err(error) = self
                .core
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!(
                            "failed to persist the provider-budget surface decision ({error}); refusing to start the model round"
                        ),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!(
                        "model input exceeds the provider window even with the context frame emptied and optional tool schemas omitted for this round ({estimated_input_tokens} > {max_input_budget} input tokens); refusing to send"
                    ),
                })
                .await;
            self.state.turn = None;
            return;
        }

        let report = surface_plan.ready_report(SurfaceReportContext {
            turn_id,
            model_round,
            surface_revision,
            estimated_input_tokens,
            input_budget_tokens: max_input_budget,
        });
        let tool_surface = surface_plan.into_snapshot(surface_revision);
        let operation_id = OperationId::new();
        let context_ack = ContextConsumptionAck {
            turn_id,
            operation_id,
            model_round,
            materialization_id: materialized.materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
        };
        let generation = self.state.generation;
        let cancel = CancellationToken::new();

        // Publish exactly once, after final packing succeeds. The provider
        // request and every later tool-call validation in this round now
        // share this immutable, round-local snapshot; failed trial packing
        // never becomes turn state.
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.tool_surface = Some(tool_surface);
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            kind: OpKind::Model,
            scope_id: None,
            tool_identity: None,
            cancel: cancel.clone(),
        });

        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
            self.state.turn = None;
            return;
        }
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ModelStarted {
                turn_id,
                operation_id,
                generation,
                surface_revision,
                model_round,
                prompt_layers: crate::prompt::prompt_layer_costs(
                    &self.assembler,
                    runtime_focus.as_ref(),
                    task_view.as_ref(),
                    progress_view.as_ref(),
                    &materialized,
                    &turn_frame,
                    &input.tool_schemas,
                ),
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
            self.state.turn = None;
            return;
        }

        let core = self.core.clone();
        let services = self.services.clone();
        let sink = LiveSink::new(
            core.event_sender(),
            core.event_sequence(),
            core.run_id(),
            turn_id,
            operation_id,
            generation,
        );
        let op_tx = op_tx.clone();
        let run_id = core.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        tokio::spawn(async move {
            let outcome = match services
                .run_model_round(
                    ModelRequest {
                        messages: input.into_messages(),
                        tools: input.tool_schemas.clone(),
                        metadata: serde_json::json!({
                            "run_id": run_id.to_string(),
                            "context_selected": materialized.selected.len(),
                            "context_approx_tokens": materialized.approx_tokens,
                            "model_round": model_round,
                            "tool_surface_revision": surface_revision,
                        }),
                        cancel: cancel.clone(),
                    },
                    &sink,
                )
                .await
            {
                Ok(output) => OperationOutcome::ModelOutput {
                    content: output.content,
                    tool_calls: output.tool_calls,
                    usage: output.usage,
                },
                Err(AgentError::Cancelled) => OperationOutcome::Cancelled,
                Err(error) => OperationOutcome::Failed {
                    message: crate::output::bound_error_message(error.to_string()),
                },
            };
            let _ = op_tx
                .send(OperationCompletion {
                    operation: OperationResult {
                        run_id,
                        turn_id,
                        task_id,
                        scope_id,
                        operation_id,
                        generation,
                        outcome,
                    },
                    kind: OpKind::Model,
                    effect: None,
                    lease: None,
                    effect_id: None,
                    argument_digest: None,
                    tool_identity: None,
                    value_completion_pending: false,
                    directive: None,
                    disposition: ToolResultDisposition::PersistObservation,
                    context_ack: Some(context_ack),
                })
                .await;
        });
    }

    fn assemble_model_input(
        &self,
        history: &MaterializedContext,
        turn_frame: &TurnFrame,
        tools: Vec<ToolSpec>,
    ) -> ModelInput {
        let (focus, task, progress) = self.runtime_prompt_focus(turn_frame);
        self.assembler.assemble(
            focus.as_ref(),
            task.as_ref(),
            progress.as_ref(),
            history,
            turn_frame,
            tools,
        )
    }

    fn runtime_prompt_focus(
        &self,
        turn_frame: &TurnFrame,
    ) -> (
        Option<FocusState>,
        Option<TaskAnchorView>,
        Option<TaskProgressView>,
    ) {
        let Some(task_id) = self.state.task_id else {
            return (None, None, None);
        };
        let Some(task) = self.state.tasks.get(task_id) else {
            return (None, None, None);
        };
        let mut focus = FocusState::for_task(task_id, task.goal.clone());
        if !turn_frame.user_message.is_empty() {
            focus.current_query = turn_frame.user_message.clone();
        }
        let turn_number = self
            .state
            .turn
            .as_ref()
            .map(|turn| turn.model_round as u64)
            .unwrap_or(0);
        let progress = self.services.project_task_progress().then(|| {
            task.resume
                .project_from_turn(turn_frame, task.anchor.revision, turn_number)
        });
        (
            Some(focus),
            Some(crate::task::task_anchor_view(&task.anchor)),
            progress,
        )
    }
}

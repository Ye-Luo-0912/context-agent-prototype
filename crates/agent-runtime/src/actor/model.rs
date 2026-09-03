use super::*;

use crate::task::COMPLETION_REPAIR_VIEW_CHARS;

fn is_required_context_body(materialized: &MaterializedContext, item: &MaterializedItem) -> bool {
    item.retention == ContextRetention::Pinned
        || materialized.required_item_ids.contains(&item.item_id)
}

fn largest_final_pack_drop_index(
    materialized: &MaterializedContext,
    items: &[MaterializedItem],
) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| !is_required_context_body(materialized, item))
        .max_by_key(|(_, item)| approx_tokens(&item.content))
        .or_else(|| {
            items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| approx_tokens(&item.content))
        })
        .map(|(index, _)| index)
}

fn final_frame_body_key(item: &MaterializedItem) -> Option<String> {
    let path = item
        .file_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    match item
        .file_revision
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(revision) => Some(format!("{path}@{revision}")),
        None => Some(path.to_string()),
    }
}

fn record_final_pack_drop(
    materialized: &mut MaterializedContext,
    dropped: &MaterializedItem,
    active_anchor_revision: u64,
) {
    // The same body may legitimately live in both the selected and the
    // foreground layer (a resource that was both scored and explicitly
    // requested). Removing one copy is not a miss while another copy of
    // the same body stays in the final frame; recording a
    // `BudgetExcluded` entry for it would misclassify a body that remains
    // visible to the model.
    let dropped_key = final_frame_body_key(dropped);
    let still_visible = materialized
        .items
        .iter()
        .chain(materialized.foreground.iter())
        .any(|item| {
            item.item_id == dropped.item_id
                || (dropped_key.is_some() && final_frame_body_key(item) == dropped_key)
        });
    if still_visible {
        return;
    }
    let required = is_required_context_body(materialized, dropped);
    let miss = ContextMaterializationMiss {
        identity: ContextMaterializationIdentity::new(
            format!("context://run/{}", dropped.item_id),
            Some(dropped.item_id),
            "runtime:final_pack",
            active_anchor_revision,
        ),
        reason: ContextMaterializationMissReason::BudgetExcluded,
    };
    if required {
        materialized.required_misses.push(miss);
    } else {
        materialized.optional_misses.push(miss);
    }
}

fn settlement_progress_views(
    base: &Option<TaskProgressView>,
    candidate: bool,
    project_settlement: bool,
    diagnostics: bool,
) -> (Option<TaskProgressView>, Option<TaskProgressView>) {
    let mut actual = base.clone();
    if candidate
        && project_settlement
        && let Some(progress) = actual.as_mut()
    {
        progress.settlement = Some(crate::task::SETTLED_CANDIDATE_PROMPT_LINE.to_string());
    }
    let treatment = if diagnostics {
        let mut treatment = base.clone();
        if candidate && let Some(progress) = treatment.as_mut() {
            progress.settlement = Some(crate::task::SETTLED_CANDIDATE_PROMPT_LINE.to_string());
        }
        treatment
    } else {
        None
    };
    (treatment, actual)
}

fn settlement_packing_projects(
    candidate: bool,
    project_settlement: bool,
    diagnostics: bool,
) -> bool {
    candidate && (project_settlement || diagnostics)
}

fn settlement_audit_enabled(candidate: bool, diagnostics: bool) -> bool {
    candidate && diagnostics
}

fn settlement_packing_requires_counterfactual(
    candidate: bool,
    project_settlement: bool,
    diagnostics: bool,
) -> bool {
    candidate && diagnostics && !project_settlement
}

fn latest_completion_gate_was_refused(frame: &TurnFrame) -> bool {
    frame.steps.iter().rev().find_map(|step| {
        let TurnFrameStep::ToolResult { output, .. } = step else {
            return None;
        };
        (output.tool_name == "task.complete").then(|| {
            !output.ok
                && output
                    .metadata
                    .get("refused")
                    .and_then(|value| value.as_str())
                    == Some("completion_gate")
        })
    }) == Some(true)
}

fn model_request_metadata(
    run_id: RunId,
    context_selected: usize,
    context_approx_tokens: usize,
    model_round: usize,
    surface_revision: u64,
    settlement_projection_audit: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut metadata = serde_json::json!({
        "run_id": run_id.to_string(),
        "context_selected": context_selected,
        "context_approx_tokens": context_approx_tokens,
        "model_round": model_round,
        "tool_surface_revision": surface_revision,
    });
    if let Some(audit) = settlement_projection_audit {
        metadata
            .as_object_mut()
            .expect("request metadata is an object")
            .insert("settlement_projection_audit".into(), audit);
    }
    metadata
}

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

        let has_external_context = match self
            .services
            .context_maintain(ContextMaintenanceTrigger::BeforeModel)
            .await
        {
            Ok(report) => {
                let has_external_context =
                    crate::execution::catalog_has_external_context(&report.diagnostics);
                if let Err(error) = self
                    .emit_context_maintained(ContextMaintenanceTrigger::BeforeModel, report)
                    .await
                {
                    // The maintenance state change landed but its audit
                    // event did not: fence the turn instead of letting the
                    // state silently outrun its journal event.
                    self.fail_round_preparation("before_model_maintained_event", error)
                        .await;
                    return;
                }
                has_external_context
            }
            Err(error) => {
                // BeforeModel maintenance may have partially applied: the
                // engine state can no longer be trusted without recovery.
                self.fail_round_preparation("before_model_maintain", error)
                    .await;
                return;
            }
        };

        self.revalidate_stored_resource_facts(&current_input).await;
        self.capture_round_snapshot(&current_input, has_external_context);
        let snapshot = self.round_snapshot().cloned();

        // Build roots before either lifecycle mechanism mutates the surface.
        // They come from exact task requirements, typed execution needs,
        // pending explicit loads and the preceding model batch whose results
        // this request will consume. No free-text action plan or fixed lease
        // duration participates.
        let lease_catalog = self.services.tool_specs();
        let task_roots = self.tool_lease_roots(&lease_catalog, &[], true, true);

        // Tool lifecycle GC remains the bounded pressure/idle backstop. The
        // same source roots protect required, pending-load and result-delivery
        // tools; task demand can restore schema readiness but never grants
        // authority.
        // It runs before lease reconciliation so a newly released schema
        // finishes this decision boundary at Warm rather than immediately
        // crossing the older Warm->Unloaded idle threshold.
        self.services.tool_gc(&task_roots);

        // A newly applied directive is a hard semantic boundary for
        // ephemeral model-load/result-delivery leases. Reconcile once here
        // so an aborted old turn or restored loaded snapshot cannot leak
        // optional schemas into the new directive. Typed/task roots survive;
        // everything else makes a Loaded->Warm transition and remains
        // exactly reloadable.
        if model_round == 1 {
            let report = self.services.tool_reconcile_leases(&task_roots);
            if report.examined_loaded_optional > 0
                && let Err(error) = self
                    .core
                    .emit_event(RuntimeEvent::ToolLeasesReconciled {
                        turn_id,
                        model_round,
                        boundary: ToolLeaseBoundary::DirectiveStart,
                        report,
                    })
                    .await
            {
                // The surface transition already landed. Without its journal
                // record the next model request would observe an unaudited
                // catalog state, so fence instead of continuing.
                self.fail_round_preparation("tool_leases_reconciled_event", error)
                    .await;
                return;
            }
        }

        let active_task = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id));
        let need_evidence = snapshot
            .as_ref()
            .map(|snap| snap.needs.evidence_needed || snap.needs.open_loop_needs_evidence)
            .unwrap_or(has_external_context);
        let verification_due = self
            .round_verification()
            .map(|projection| projection.due)
            .unwrap_or(false);
        let turn_intent = active_task
            .map(|task| task.turn_intent.as_str())
            .filter(|intent| !intent.is_empty());
        let completion_requested =
            turn_intent.is_some_and(crate::execution::ExecutionState::turn_requests_complete);
        let completion_repair_due = self
            .state
            .turn
            .as_ref()
            .is_some_and(|turn| latest_completion_gate_was_refused(&turn.turn_frame));
        let completion_repair_readiness = completion_repair_due
            .then(|| self.completion_readiness(CompletionIntent::ModelProposal, None));
        let completion_repair_blockers = completion_repair_readiness
            .as_ref()
            .map(CompletionReadiness::applicable_blockers)
            .unwrap_or_default();
        let has_failures = snapshot
            .as_ref()
            .map(|snap| snap.needs.unresolved_failure)
            .unwrap_or(false);
        let (task_requirement_revision, mut requirements) = active_task
            .map(|task| {
                (
                    Some(task.tool_requirements.revision),
                    task.tool_requirements.entries.clone(),
                )
            })
            .unwrap_or((None, Vec::new()));
        // Ending a model turn is implicit; closing the durable task is a
        // separate lifecycle transition. `task.complete` is always present
        // in the v5 catalog: this requirement only prefers it on the
        // surface when the current directive explicitly requests closure,
        // so ordinary work does not accidentally erase task affinity.
        if completion_requested && !completion_repair_due {
            requirements.push(ToolSurfaceRequirement {
                tool_name: "task.complete".into(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: "current user directive explicitly requests task closure".into(),
            });
        }
        // A refused completion starts a bounded repair episode. Re-derive the
        // current stage every decision and prefer only its resolver; a repair
        // helper may never abort the round if loading or packing it fails.
        if completion_repair_due
            && !completion_repair_blockers
                .iter()
                .any(|blocker| blocker.requires_operator_repair())
        {
            let catalog = self.services.tool_catalog();
            let progress_blocked = completion_repair_blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    CompletionBlocker::OpenLoops { .. } | CompletionBlocker::NextActionPending
                )
            });
            let execution_blocked = completion_repair_blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    CompletionBlocker::ExecutionObligations { .. }
                        | CompletionBlocker::FailedCommands { .. }
                )
            });
            let proof_blocked = completion_repair_blockers.iter().any(|blocker| {
                matches!(
                    blocker,
                    CompletionBlocker::VerificationNotCurrent
                        | CompletionBlocker::AcceptanceUncovered { .. }
                )
            });
            let resolver = if progress_blocked {
                Some((
                    "task.manage",
                    "completion repair: update only resolved open loops/next action",
                ))
            } else if execution_blocked {
                None // obligation source tools are already rooted by ExecutionState
            } else if proof_blocked
                && completion_repair_readiness
                    .as_ref()
                    .and_then(|readiness| self.current_completion_proof_route(readiness))
                    .is_some()
            {
                Some((
                    "verify.run",
                    "completion repair: refresh the exact host verifier after workspace calls",
                ))
            } else {
                None
            };
            if let Some((tool_name, reason)) = resolver
                && catalog.iter().any(|entry| entry.name == tool_name)
            {
                requirements.push(ToolSurfaceRequirement {
                    tool_name: tool_name.into(),
                    demand: ToolSurfaceDemand::PreferSurface,
                    reason: reason.into(),
                });
            }
            if !progress_blocked
                && !execution_blocked
                && !proof_blocked
                && completion_repair_blockers.is_empty()
            {
                requirements.push(ToolSurfaceRequirement {
                    tool_name: "task.complete".into(),
                    demand: ToolSurfaceDemand::PreferSurface,
                    reason: "completion repair: current blockers are resolved".into(),
                });
            }
        }
        // Advisory completion-opportunity lease (default off): while a
        // derived lease is outstanding, this ONE decision
        // sees `task.complete` preferred on its surface. The model still
        // chooses; the lease dies with the decision.
        if self
            .state
            .turn
            .as_ref()
            .is_some_and(|turn| turn.opportunity_lease.is_some())
        {
            requirements.push(crate::opportunity::opportunity_surface_requirement());
        }
        // A trusted `parent_path_not_found` from the previous batch proved
        // the recovery contract requires topology mutation. Prefer the exact
        // host-owned `fs.mkdir` for ONE decision (explicit provenance, never
        // a model self-load); unrelated missing reads never set this state.
        if let Some(request) = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.recovery_surface_request.as_ref())
        {
            requirements.push(ToolSurfaceRequirement {
                tool_name: request.tool_name.clone(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: "typed recovery: missing parent requires directory creation".into(),
            });
        }
        // Verification is source-affine once a trusted verifier has
        // produced a reusable result for this exact task anchor. Keep that
        // concrete schema available first; the semantic-role fallback below
        // is used only when the source is absent from the current catalog.
        let verification_source_tools = if verification_due {
            active_task
                .and_then(|task| {
                    self.state.turn.as_ref().map(|turn| {
                        turn.execution
                            .verification_source_tools(task.anchor.revision)
                    })
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        requirements.extend(verification_source_tools.iter().map(|tool_name| {
            ToolSurfaceRequirement {
                tool_name: tool_name.clone(),
                demand: ToolSurfaceDemand::PreferSurface,
                reason: "trusted verifier source for current task anchor".into(),
            }
        }));

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
                let _ = self.services.tool_load_for_lease(&requirement.tool_name);
            }
        }
        // Item 24: `context.manage` is catalog-only until NeedEvidence.
        // Load it before the candidate snapshot so policy can PreferSurface it.
        if need_evidence && !visible_names.contains(CONTEXT_MANAGE) {
            let _ = self.services.tool_load_for_lease(CONTEXT_MANAGE);
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
        let exact_verifier_available = verification_source_tools
            .iter()
            .any(|tool_name| candidate_names.contains(tool_name));

        // Derive typed tool roots from execution need → catalog roles
        // (not hard-coded tool names), then merge them into the explicit
        // requirement set. Derivation is a pure function of the safe-point
        // state and only names tools that exist in the candidate catalog;
        // the explicit task-owned set stays the authority (higher demand
        // ranks win).
        let anchor = active_task.map(|task| &task.anchor);
        let active_tool = self.state.active_tool.as_deref();
        requirements.extend(crate::policy::derive_task_roots(
            crate::policy::TaskRootInput {
                anchor,
                focus_goal: active_task.map(|task| task.goal.as_str()),
                active_tool,
                catalog: &candidates.specs,
                verification_due: verification_due && !exact_verifier_available,
                turn_intent,
                has_failures,
                has_external_context,
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
        // The recovery-derived requirement enters as a task-style demand;
        // relabel its provenance so report rows answer "why" truthfully.
        if let Some(request) = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.recovery_surface_request.as_ref())
        {
            surface_plan.mark_recovery_tools(&std::collections::HashSet::from([request
                .tool_name
                .clone()]));
        }
        // One-decision source lifetime: the recovery request is consumed by
        // this surface and cannot re-arm the next decision, whether or not
        // the model calls the tool.
        if let Some(turn) = self.state.turn.as_mut() {
            turn.recovery_surface_request = None;
        }
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
                    self.fail_round_preparation("surface_revision", error).await;
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
                // The refusal decision itself could not be journaled: the
                // audit trail must not lose why this round never started.
                self.fail_round_preparation("tool_surface_planned_event", error)
                    .await;
                return;
            }
            // Deliberate refusal, not a fault: settle the applied input and
            // drop the turn without fencing.
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: "the active task requires a tool that is unavailable; refusing to start the model round"
                        .into(),
                })
                .await;
            self.settle_aborted_turn().await;
            return;
        }

        if surface_plan.mandatory_schema_tokens() > MAX_TOOL_SURFACE_TOKENS {
            let surface_revision = match self.issue_surface_revision() {
                Ok(revision) => revision,
                Err(error) => {
                    self.fail_round_preparation("surface_revision", error).await;
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
                // The refusal decision itself could not be journaled: the
                // audit trail must not lose why this round never started.
                self.fail_round_preparation("tool_surface_planned_event", error)
                    .await;
                return;
            }
            // Deliberate refusal, not a fault: settle the applied input and
            // drop the turn without fencing.
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
            self.settle_aborted_turn().await;
            return;
        }

        // 发送窗口与打包窗口分离：SWE-bench 工具轮的 turn frame 必须
        // 能发出去；C 的 working set 仍按内核 pack cap 收。未声明
        // provider 窗口时两者都回退到内核 budget（旧行为）。
        // Token 计量的是确定性 checkpointing 之后真正上线的协议视图
        // （保留尾部 + 有界 checkpoint 注记），与装配器一致。
        let capabilities = self.services.model_capabilities();
        let turn_frame_tokens = approx_layer_tokens(
            &turn_frame.checkpointed_messages(agent_contracts::TURN_FRAME_KEEP_EXCHANGES),
        );
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
        let (runtime_focus, task_view, base_progress_view, settlement_candidate) =
            self.runtime_prompt_focus(&turn_frame).await;
        let project_settlement = self.services.project_settlement();
        let settlement_projection_diagnostics = self.services.settlement_projection_diagnostics();
        // Product requests are budgeted against the arm they actually send.
        // Only an explicitly paired causal diagnostic uses the common,
        // treatment-sized envelope needed to prevent a one-line treatment
        // from indirectly changing context or tool selection.
        let mut budget_progress_view = base_progress_view.clone();
        if settlement_packing_projects(
            settlement_candidate,
            project_settlement,
            settlement_projection_diagnostics,
        ) && let Some(progress) = budget_progress_view.as_mut()
        {
            progress.settlement = Some(crate::task::SETTLED_CANDIDATE_PROMPT_LINE.to_string());
        }
        let runtime_focus_frame_tokens = crate::prompt::focus_frame_tokens(
            runtime_focus.as_ref(),
            task_view.as_ref(),
            budget_progress_view.as_ref(),
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
        let foreground_resources = self.foreground_resource_hints(&turn_frame, &current_input);
        let protocol_bodies = self.eligible_protocol_bodies();
        let visible_body_identities = crate::prompt::visible_body_identities_for_request(
            &turn_frame,
            base_progress_view.as_ref(),
            &protocol_bodies,
        );
        let context_budget = model_budget.context_frame_budget;
        let materialized = match self
            .services
            .context_materialize(ContextQuery {
                current_input: current_input.clone(),
                budget_tokens: context_budget,
                hints: ContextHints {
                    max_selected_items: Some(CONTEXT_CONSUMPTION_ACK_ITEM_CAP),
                    anchor_roots,
                    task: task_view.clone(),
                    checked_files: base_progress_view
                        .as_ref()
                        .map(|view| view.checked_files.clone())
                        .unwrap_or_default(),
                    visible_body_identities,
                    foreground_resources,
                },
            })
            .await
        {
            Ok(materialized) => materialized,
            Err(error) => {
                // Materialize advances engine clocks and may run through the
                // process adapter: a failure here leaves the engine state
                // unprovable, so fence instead of retrying blind.
                self.fail_round_preparation("context_materialize", error)
                    .await;
                return;
            }
        };
        if let Err(error) = materialized.validate_materialization() {
            // Concrete and test context engines share this in-process trust
            // boundary. Reject an oversized, malformed or unowned frame
            // before it can be cloned into the durable event stream or sent
            // to the provider.
            self.fail_round_preparation("context_materialization", error)
                .await;
            return;
        }
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
        // A miss discovered by this exact final materialization invalidates
        // the pre-materialization settlement observation for the request
        // being prepared. Packing may still reserve the line, but neither
        // arm claims the task is settled until all required bodies landed.
        let mut settlement_candidate =
            settlement_candidate && materialized.required_misses.is_empty();
        let (mut treatment_progress_view, mut progress_view) = settlement_progress_views(
            &base_progress_view,
            settlement_candidate,
            project_settlement,
            settlement_projection_diagnostics,
        );
        let mut packing_progress_view = if settlement_projection_diagnostics {
            treatment_progress_view.clone()
        } else {
            progress_view.clone()
        };
        let active_anchor_revision = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| task.anchor.revision)
            .unwrap_or_default();
        let (mut input, mut body_cache_stats) = self.assemble_model_input(
            runtime_focus.as_ref(),
            task_view.as_ref(),
            progress_view.as_ref(),
            &materialized,
            &turn_frame,
            surface_plan.specs().to_vec(),
        );
        // A second packed request exists only for the diagnostic off arm.
        // Ordinary product off/on paths measure and trim `input` directly;
        // they neither assemble nor clone a second ModelInput.
        let mut packing_input = if settlement_packing_requires_counterfactual(
            settlement_candidate,
            project_settlement,
            settlement_projection_diagnostics,
        ) {
            Some(
                self.assemble_model_input(
                    runtime_focus.as_ref(),
                    task_view.as_ref(),
                    packing_progress_view.as_ref(),
                    &materialized,
                    &turn_frame,
                    surface_plan.specs().to_vec(),
                )
                .0,
            )
        } else {
            None
        };
        let assembled_total = |input: &ModelInput| {
            approx_layer_tokens(&input.into_messages()) + approx_layer_tokens(&input.tool_schemas)
        };
        let packed_total = |input: &ModelInput, packing: &Option<ModelInput>| {
            assembled_total(packing.as_ref().unwrap_or(input))
        };
        while packed_total(&input, &packing_input) > max_input_budget
            && !materialized.items.is_empty()
        {
            // Drop the largest optional item first. If only mandatory
            // bodies remain, the hard provider budget still wins, but that
            // removal becomes an explicit BudgetExcluded completion blocker.
            let drop_index = largest_final_pack_drop_index(&materialized, &materialized.items);
            let Some(drop_index) = drop_index else {
                break;
            };
            let dropped = materialized.items.remove(drop_index);
            record_final_pack_drop(&mut materialized, &dropped, active_anchor_revision);
            materialized
                .selected
                .retain(|selection| selection.item_id != dropped.item_id);
            materialized.approx_tokens = materialized
                .approx_tokens
                .saturating_sub(approx_tokens(&dropped.content));
            (input, body_cache_stats) = self.assemble_model_input(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
            if packing_input.is_some() {
                packing_input = Some(
                    self.assemble_model_input(
                        runtime_focus.as_ref(),
                        task_view.as_ref(),
                        packing_progress_view.as_ref(),
                        &materialized,
                        &turn_frame,
                        surface_plan.specs().to_vec(),
                    )
                    .0,
                );
            }
        }
        while packed_total(&input, &packing_input) > max_input_budget
            && !materialized.foreground.is_empty()
        {
            let drop_index = largest_final_pack_drop_index(&materialized, &materialized.foreground);
            let Some(drop_index) = drop_index else {
                break;
            };
            let dropped = materialized.foreground.remove(drop_index);
            record_final_pack_drop(&mut materialized, &dropped, active_anchor_revision);
            materialized.approx_tokens = materialized
                .approx_tokens
                .saturating_sub(approx_tokens(&dropped.content));
            (input, body_cache_stats) = self.assemble_model_input(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
            if packing_input.is_some() {
                packing_input = Some(
                    self.assemble_model_input(
                        runtime_focus.as_ref(),
                        task_view.as_ref(),
                        packing_progress_view.as_ref(),
                        &materialized,
                        &turn_frame,
                        surface_plan.specs().to_vec(),
                    )
                    .0,
                );
            }
        }

        // The context frame is empty but the fixed layers still overshoot:
        // omit optional schemas from this round's snapshot only. Provider
        // token pressure must never unload a catalog entry, bump its
        // generation or make a later, larger-budget round forget the tool.
        // The trimmed snapshot remains the one source for prompt assembly,
        // accounting and tool-call validation in this round.
        while packed_total(&input, &packing_input) > max_input_budget {
            if surface_plan.omit_largest_for_provider_budget().is_none() {
                break;
            }
            (input, body_cache_stats) = self.assemble_model_input(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
            if packing_input.is_some() {
                packing_input = Some(
                    self.assemble_model_input(
                        runtime_focus.as_ref(),
                        task_view.as_ref(),
                        packing_progress_view.as_ref(),
                        &materialized,
                        &turn_frame,
                        surface_plan.specs().to_vec(),
                    )
                    .0,
                );
            }
        }

        // Runtime trimming itself may have displaced a required body and
        // appended `BudgetExcluded`. Revoke the projected fact on the exact
        // request being sent. Once the candidate is revoked there is no
        // treatment exposure to compare, so a diagnostic off-arm probe is
        // dropped as well instead of retaining a second large input.
        if settlement_candidate && !materialized.required_misses.is_empty() {
            settlement_candidate = false;
            (treatment_progress_view, progress_view) = settlement_progress_views(
                &base_progress_view,
                false,
                project_settlement,
                settlement_projection_diagnostics,
            );
            packing_progress_view = if settlement_projection_diagnostics {
                treatment_progress_view.clone()
            } else {
                progress_view.clone()
            };
            (input, body_cache_stats) = self.assemble_model_input(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
            );
            packing_input = if settlement_packing_requires_counterfactual(
                settlement_candidate,
                project_settlement,
                settlement_projection_diagnostics,
            ) {
                Some(
                    self.assemble_model_input(
                        runtime_focus.as_ref(),
                        task_view.as_ref(),
                        packing_progress_view.as_ref(),
                        &materialized,
                        &turn_frame,
                        surface_plan.specs().to_vec(),
                    )
                    .0,
                )
            } else {
                None
            };
        }

        if let Err(error) = materialized.validate_materialization() {
            self.fail_round_preparation("final_context_materialization", error)
                .await;
            return;
        }

        let estimated_input_tokens = assembled_total(&input);
        let packing_input_tokens = packed_total(&input, &packing_input);
        // 正文恢复账目出账（增量）。eligible 是最终
        // 组装的真实 checkpoint demand；失效/超限计数是自上一条账目
        // 以来的累计，drain 后归零。
        // 只记账不设障：事件失败不影响本轮准备。
        if let Some(turn) = self.state.turn.as_mut() {
            let deltas = turn.protocol_bodies.drain_deltas();
            let _ = self
                .core
                .emit_event(RuntimeEvent::ProtocolBodyCacheStats {
                    eligible: body_cache_stats.eligible,
                    hit: body_cache_stats.restored,
                    miss: body_cache_stats
                        .eligible
                        .saturating_sub(body_cache_stats.restored),
                    invalidated: deltas.invalidated,
                    suspended: deltas.suspended,
                    oversize: deltas.oversize,
                    restored_body_tokens: body_cache_stats.restored_body_tokens,
                })
                .await;
        }
        let surface_revision = match self.issue_surface_revision() {
            Ok(revision) => revision,
            Err(error) => {
                self.fail_round_preparation("surface_revision", error).await;
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
            // The consumption ack below references this preview; without
            // the durable ContextPrepared record the round must not start.
            self.fail_round_preparation("context_prepared_event", error)
                .await;
            return;
        }
        if (!materialized.required_misses.is_empty() || !materialized.optional_misses.is_empty())
            && let Err(error) = self
                .core
                .emit_event(RuntimeEvent::ContextDegraded {
                    turn_id,
                    model_round,
                    materialization_id: materialized.materialization_id,
                    required_misses: materialized.required_misses.clone(),
                    optional_misses: materialized.optional_misses.clone(),
                })
                .await
        {
            self.fail_round_preparation("context_degraded_event", error)
                .await;
            return;
        }

        if packing_input_tokens > max_input_budget {
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
                // The refusal decision itself could not be journaled: the
                // audit trail must not lose why this round never started.
                self.fail_round_preparation("tool_surface_planned_event", error)
                    .await;
                return;
            }
            // Deliberate refusal, not a fault: settle the applied input and
            // drop the turn without fencing.
            let _ = self
                .core
                .emit_event(RuntimeEvent::Failure {
                    class: RuntimeFailureClass::InputBudget,
                    retryable: false,
                    message: format!(
                        "model input exceeds the provider window even with the context frame emptied and optional tool schemas omitted for this round ({packing_input_tokens} > {max_input_budget} conservatively packed input tokens); refusing to send"
                    ),
                })
                .await;
            self.settle_aborted_turn().await;
            return;
        }

        // Live causal proof from this exact runtime state and final packed
        // surface. Both counterfactuals reuse the same materialized bodies,
        // TurnFrame and schemas; the shared comparator removes only the
        // declared settlement line. The proof rides request metadata for an
        // observational eval transport and is not part of provider messages
        // or tool schemas.
        let settlement_projection_audit = if settlement_audit_enabled(
            settlement_candidate,
            settlement_projection_diagnostics,
        ) {
            // Reuse the actual request and (for the diagnostic off arm) the
            // treatment-sized packing probe. The on arm constructs only its
            // missing baseline counterfactual. Diagnostics therefore prove
            // both shapes without assembling either shape twice.
            let baseline_counterfactual;
            let (baseline_input, treatment_input) = if project_settlement {
                baseline_counterfactual = self
                    .assemble_model_input(
                        runtime_focus.as_ref(),
                        task_view.as_ref(),
                        base_progress_view.as_ref(),
                        &materialized,
                        &turn_frame,
                        surface_plan.specs().to_vec(),
                    )
                    .0;
                (&baseline_counterfactual, &input)
            } else {
                let Some(treatment_input) = packing_input.as_ref() else {
                    self.fail_round_preparation(
                        "settlement_projection_audit",
                        AgentError::Internal(
                            "diagnostic off-arm lost its treatment packing input".into(),
                        ),
                    )
                    .await;
                    return;
                };
                (&input, treatment_input)
            };
            match crate::prompt::compare_settlement_projection(baseline_input, treatment_input) {
                Ok(audit) if audit.passed => match serde_json::to_value(audit) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        self.fail_round_preparation(
                            "settlement_projection_audit",
                            AgentError::Internal(format!(
                                "settlement projection audit serialization failed: {error}"
                            )),
                        )
                        .await;
                        return;
                    }
                },
                Ok(audit) => {
                    self.fail_round_preparation(
                        "settlement_projection_audit",
                        AgentError::Internal(format!(
                            "settlement treatment changed more than its declared fact (occurrences={})",
                            audit.settlement_occurrences
                        )),
                    )
                    .await;
                    return;
                }
                Err(error) => {
                    self.fail_round_preparation("settlement_projection_audit", error)
                        .await;
                    return;
                }
            }
        } else {
            None
        };

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
            // Foreground bodies the prompt rendered this round: the model
            // saw them, so consumption observability must record them
            // (weak signal only; engines must not change residency).
            foreground_item_ids: materialized
                .foreground
                .iter()
                .map(|item| item.item_id)
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
            self.fail_round_preparation("tool_surface_planned_event", error)
                .await;
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
                turn_checkpoint: input
                    .turn_checkpoint
                    .as_ref()
                    .map(agent_contracts::TurnCheckpointStats::from)
                    .unwrap_or_default(),
                prompt_layers: crate::prompt::prompt_layer_costs_with_catalog(
                    &self.assembler,
                    runtime_focus.as_ref(),
                    task_view.as_ref(),
                    progress_view.as_ref(),
                    &materialized,
                    &turn_frame,
                    &input.tool_schemas,
                    &self.services.tool_catalog(),
                    &self.eligible_protocol_bodies(),
                ),
            })
            .await
        {
            // The operation is already installed in the turn; without the
            // durable ModelStarted the live stream and every later event
            // lose their envelope cursor. Fence instead of sending.
            self.fail_round_preparation("model_started_event", error)
                .await;
            return;
        }

        // Completion proposals from this operation are evaluated against
        // exactly the final packed frame whose start event is now durable.
        self.record_context_requirement_observation(materialized.required_misses.total());

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
        let request_metadata = model_request_metadata(
            run_id,
            materialized.selected.len(),
            materialized.approx_tokens,
            model_round,
            surface_revision,
            settlement_projection_audit,
        );
        tokio::spawn(async move {
            let outcome = match services
                .run_model_round(
                    ModelRequest {
                        messages: input.into_messages(),
                        tools: input.tool_schemas.clone(),
                        metadata: request_metadata,
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
                Err(error) => {
                    let (class, retryable) = Self::classify_model_failure(&error);
                    OperationOutcome::Failed {
                        class,
                        retryable,
                        message: crate::output::bound_error_message(error.to_string()),
                    }
                }
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
                    attribution: None,
                    verification_call: None,
                    tool_identity: None,
                    value_completion_pending: false,
                    recovery_required: None,
                    directive: None,
                    disposition: ToolResultDisposition::PersistObservation,
                    context_ack: Some(context_ack),
                })
                .await;
        });
    }

    fn classify_model_failure(error: &AgentError) -> (RuntimeFailureClass, bool) {
        match error {
            AgentError::Transport { retryable, .. } => {
                (RuntimeFailureClass::ProviderTransport, *retryable)
            }
            AgentError::TransportRetryAfter { .. } => {
                (RuntimeFailureClass::ProviderTransport, true)
            }
            AgentError::ModelOutputLimit { .. } => (RuntimeFailureClass::ModelOutputLimit, false),
            AgentError::Model(_) | AgentError::ModelProtocol { .. } => {
                (RuntimeFailureClass::Model, false)
            }
            _ => (RuntimeFailureClass::Runtime, false),
        }
    }

    fn assemble_model_input(
        &self,
        focus: Option<&FocusState>,
        task: Option<&TaskAnchorView>,
        progress: Option<&TaskProgressView>,
        history: &MaterializedContext,
        turn_frame: &TurnFrame,
        tools: Vec<ToolSpec>,
    ) -> (ModelInput, crate::prompt::ProtocolBodyAssemblyStats) {
        // 当轮正文缓存的可回注行交给组装器。休眠
        // 条目只有在事实表里同 path@digest 重新 Fresh（BeforeModel 重
        // 验证通过）时才恢复资格；是否回注由组装器再核对 checkpoint
        // 截断 + Fresh 事实一致。
        let protocol_bodies = self.eligible_protocol_bodies();
        self.assembler.assemble_with_catalog_stats(
            focus,
            task,
            progress,
            history,
            turn_frame,
            tools,
            &self.services.tool_catalog(),
            &protocol_bodies,
        )
    }

    fn eligible_protocol_bodies(&self) -> Vec<(String, String)> {
        self.state
            .turn
            .as_ref()
            .map(|turn| {
                let fresh_identities: Vec<(String, String)> = turn
                    .execution
                    .checked_files
                    .iter()
                    .filter(|fact| fact.freshness == agent_contracts::ResourceFreshness::Fresh)
                    .map(|fact| (fact.path.clone(), fact.digest.clone()))
                    .collect();
                turn.protocol_bodies.eligible_rows(&fresh_identities)
            })
            .unwrap_or_default()
    }

    /// Exact schema roots for one safe point. This is a projection of
    /// existing authority/facts plus explicit model calls; it never chooses
    /// an action, command or argument for the model.
    pub(super) fn tool_lease_roots(
        &self,
        catalog: &[ToolSpec],
        decision_calls: &[ToolCall],
        include_turn_leases: bool,
        include_active_tool: bool,
    ) -> Vec<String> {
        let active_task = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id));
        let snapshot = self.round_snapshot();
        let mut roots: Vec<String> = active_task
            .map(|task| {
                task.tool_requirements
                    .entries
                    .iter()
                    .map(|requirement| requirement.tool_name.clone())
                    .collect()
            })
            .unwrap_or_default();

        let verification_due = snapshot
            .map(|round| round.verification.due)
            .unwrap_or(false);
        let verification_source_tools: Vec<String> = if verification_due {
            active_task
                .and_then(|task| {
                    self.state.turn.as_ref().map(|turn| {
                        turn.execution
                            .verification_source_tools(task.anchor.revision)
                    })
                })
                .unwrap_or_default()
                .into_iter()
                .filter(|tool_name| catalog.iter().any(|spec| spec.name == *tool_name))
                .collect()
        } else {
            Vec::new()
        };
        roots.extend(verification_source_tools.iter().cloned());

        let derived = crate::policy::derive_task_roots(crate::policy::TaskRootInput {
            anchor: active_task.map(|task| &task.anchor),
            focus_goal: active_task.map(|task| task.goal.as_str()),
            active_tool: include_active_tool
                .then_some(self.state.active_tool.as_deref())
                .flatten(),
            catalog,
            verification_due: verification_due && verification_source_tools.is_empty(),
            turn_intent: active_task
                .map(|task| task.turn_intent.as_str())
                .filter(|intent| !intent.is_empty()),
            has_failures: snapshot
                .map(|round| round.needs.unresolved_failure)
                .unwrap_or(false),
            has_external_context: snapshot
                .map(|round| round.needs.evidence_needed)
                .unwrap_or(false),
        });
        roots.extend(derived.into_iter().map(|requirement| requirement.tool_name));

        if snapshot.is_some_and(|round| {
            round.needs.evidence_needed || round.needs.open_loop_needs_evidence
        }) {
            roots.push(CONTEXT_MANAGE.to_string());
        }
        if include_turn_leases && let Some(turn) = self.state.turn.as_ref() {
            roots.extend(turn.pending_loaded_tools.iter().cloned());
            roots.extend(turn.result_delivery_tools.iter().cloned());
            // Unresolved obligations keep their exact source tool surfaced:
            // the ledger recorded a trusted association when the row opened,
            // and this derived view releases it exactly when the row dies.
            let obligation_source_tools: Vec<String> = turn
                .execution
                .obligation_source_tools()
                .into_iter()
                .filter(|tool_name| catalog.iter().any(|spec| spec.name == *tool_name))
                .collect();
            roots.extend(obligation_source_tools.iter().cloned());
        }
        roots.extend(decision_calls.iter().map(|call| call.name.clone()));
        roots.sort();
        roots.dedup();
        roots
    }

    /// Settle optional schema leases after one successful model decision.
    /// The decision consumes the previous result-delivery lease. A pending
    /// explicit load is consumed only when that exact tool is called, so
    /// sequential loads form a task-local cohort instead of evicting each
    /// other at adjacent decisions. An empty decision ends the turn and
    /// releases every unused pending load. Reconciliation happens before
    /// dispatch, while the actor is at a surface-safe boundary.
    pub(super) async fn reconcile_model_decision_leases(
        &mut self,
        calls: &[ToolCall],
    ) -> AgentResult<()> {
        let Some((turn_id, model_round, catalog)) = self.state.turn.as_ref().map(|turn| {
            (
                turn.turn_id,
                turn.model_round,
                turn.tool_surface
                    .as_ref()
                    .map(|surface| surface.specs.clone())
                    .unwrap_or_default(),
            )
        }) else {
            return Ok(());
        };
        let pending_loaded_tools = self
            .state
            .turn
            .as_ref()
            .map(|turn| {
                if calls.is_empty() {
                    Vec::new()
                } else {
                    turn.pending_loaded_tools
                        .iter()
                        .filter(|name| {
                            !calls.iter().any(|call| call.name.as_str() == name.as_str())
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                }
            })
            .unwrap_or_default();
        let mut roots = self.tool_lease_roots(&catalog, calls, false, false);
        roots.extend(pending_loaded_tools.iter().cloned());
        roots.sort();
        roots.dedup();
        let report = self.services.tool_reconcile_leases(&roots);
        if report.examined_loaded_optional > 0 {
            self.core
                .emit_event(RuntimeEvent::ToolLeasesReconciled {
                    turn_id,
                    model_round,
                    boundary: ToolLeaseBoundary::ModelDecision,
                    report,
                })
                .await?;
        }

        let mut delivery: Vec<String> = calls.iter().map(|call| call.name.clone()).collect();
        delivery.sort();
        delivery.dedup();
        if let Some(turn) = self.state.turn.as_mut() {
            turn.pending_loaded_tools = pending_loaded_tools;
            turn.result_delivery_tools = delivery;
        }
        // The model decision just consumed the prior active tool's result.
        // New calls establish their own active identity when dispatched.
        self.state.active_tool = None;
        Ok(())
    }

    async fn revalidate_stored_resource_facts(&mut self, current_query: &str) {
        let Some(oracle) = self.services.artifact_workspace() else {
            return;
        };
        let priority_body_identities = self
            .state
            .turn
            .as_ref()
            .map(|turn| crate::prompt::checkpoint_spilled_body_identities(&turn.turn_frame))
            .unwrap_or_default();
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.execution
            .revalidate_with_priority(
                oracle as &dyn ResourceVersionOracle,
                current_query,
                &priority_body_identities,
            )
            .await;
    }

    fn capture_round_snapshot(&mut self, current_input: &str, has_external_context: bool) {
        let (focus_goal, anchor) = match self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
        {
            Some(task) if task.status != crate::task::TaskStatus::Completed => {
                (Some(task.goal.clone()), Some(task.anchor.clone()))
            }
            _ => (None, None),
        };
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.round_snapshot = Some(crate::execution::RoundExecutionSnapshot::capture(
            &mut turn.execution,
            current_input,
            focus_goal.as_deref(),
            anchor.as_ref(),
            has_external_context,
        ));
    }

    fn round_snapshot(&self) -> Option<&crate::execution::RoundExecutionSnapshot> {
        self.state
            .turn
            .as_ref()
            .and_then(|turn| turn.round_snapshot.as_ref())
    }

    fn round_verification(&self) -> Option<&crate::execution::VerificationProjection> {
        self.round_snapshot().map(|snapshot| &snapshot.verification)
    }

    fn foreground_resource_hints(
        &self,
        _turn_frame: &TurnFrame,
        current_input: &str,
    ) -> Vec<ResourceKey> {
        if let Some(snapshot) = self.round_snapshot() {
            return snapshot.foreground_resources.clone();
        }
        let Some(turn) = self.state.turn.as_ref() else {
            let Some(task_id) = self.state.task_id else {
                return Vec::new();
            };
            let Some(task) = self.state.tasks.get(task_id) else {
                return Vec::new();
            };
            return task.resume.foreground_resources(current_input);
        };
        turn.execution.foreground_resources(current_input)
    }

    async fn runtime_prompt_focus(
        &self,
        turn_frame: &TurnFrame,
    ) -> (
        Option<FocusState>,
        Option<TaskAnchorView>,
        Option<TaskProgressView>,
        bool,
    ) {
        let Some(task_id) = self.state.task_id else {
            return (None, None, None, false);
        };
        let Some(task) = self.state.tasks.get(task_id) else {
            return (None, None, None, false);
        };
        let mut focus = FocusState::for_task(task_id, task.goal.clone());
        if !turn_frame.user_message.is_empty() {
            focus.current_query = turn_frame.user_message.clone();
        }
        let mut progress = if self.services.project_task_progress() {
            if let Some(snapshot) = self.round_snapshot() {
                Some(snapshot.progress.clone())
            } else if let Some(turn) = self.state.turn.as_ref() {
                Some(turn.execution.view())
            } else {
                Some(task.resume.view())
            }
        } else {
            None
        };
        if let Some(progress) = progress.as_mut().filter(|_| {
            self.state.turn.as_ref().is_some_and(|turn| {
                latest_completion_gate_was_refused(&turn.turn_frame)
                    || turn.execution.completion_repair.is_some()
            })
        }) {
            let readiness = self.completion_readiness(CompletionIntent::ModelProposal, None);
            let rendered = self
                .state
                .turn
                .as_ref()
                .and_then(|turn| turn.execution.completion_repair.as_ref())
                .filter(|record| record.matches_basis(&readiness))
                .map(|record| record.text.clone())
                .unwrap_or_else(|| {
                    // A durable stage whose basis drifted is stale; derive
                    // the current stage from readiness for this decision.
                    // A drifted basis is a fresh stage, so the refusal count
                    // starts at zero.
                    let (_, rendered) = self.completion_repair_plan(&readiness, 0);
                    rendered
                });
            progress.completion_repair =
                Some(bounded_preview(&rendered, COMPLETION_REPAIR_VIEW_CHARS));
        }
        // Advisory completion-opportunity (default off): project the bounded
        // closure statement only while the one-decision lease is live.
        if let Some(progress) = progress.as_mut().filter(|_| {
            self.state
                .turn
                .as_ref()
                .is_some_and(|turn| turn.opportunity_lease.is_some())
        }) {
            progress.completion_opportunity =
                Some(crate::opportunity::OPPORTUNITY_PROMPT_LINE.to_string());
        }
        // Compute the joined fact independently of the experiment switch.
        // The caller builds baseline/treatment frames from this same value,
        // packs against the larger frame, and sends only the selected arm.
        let settlement_candidate = progress.is_some()
            && match self.state.turn.as_ref() {
                Some(turn) => {
                    self.task_settlement_label(&turn.execution)
                        == agent_contracts::SettlementLabel::SettledCandidate
                }
                None => {
                    self.task_settlement_label(&task.resume)
                        == agent_contracts::SettlementLabel::SettledCandidate
                }
            };
        (
            Some(focus),
            Some(crate::task::task_anchor_view(&task.anchor)),
            progress,
            settlement_candidate,
        )
    }
}

#[cfg(test)]
mod failure_class_tests {
    use super::*;

    fn context_item(retention: ContextRetention, content: &str) -> MaterializedItem {
        MaterializedItem {
            item_id: agent_contracts::ContextItemId::new(),
            kind: agent_contracts::ContextKind::Note,
            scope: agent_contracts::ContextScope::Task,
            attention: agent_contracts::AttentionState::Active,
            semantic: agent_contracts::SemanticState::Live,
            retention,
            content: content.into(),
            source: None,
            file_path: None,
            file_revision: None,
            partial_body: false,
        }
    }

    fn completion_result(ok: bool, refused: Option<&str>) -> ToolOutput {
        ToolOutput {
            call_id: "completion-call".into(),
            tool_name: "task.complete".into(),
            ok,
            summary: "completion".into(),
            model_content: "completion".into(),
            artifact_ref: None,
            metadata: refused
                .map(|refused| serde_json::json!({"refused": refused}))
                .unwrap_or_else(|| serde_json::json!({"accepted": true})),
        }
    }

    #[test]
    fn only_the_latest_completion_result_arms_repair() {
        let mut frame = TurnFrame::new("task");
        frame.push_tool_result(
            completion_result(false, Some("completion_gate")),
            None,
            agent_contracts::ToolExecutionFacts::default(),
        );
        assert!(latest_completion_gate_was_refused(&frame));

        let mut repair_action = completion_result(true, None);
        repair_action.tool_name = "task.manage".into();
        repair_action.metadata = serde_json::json!({});
        frame.push_tool_result(
            repair_action,
            None,
            agent_contracts::ToolExecutionFacts::default(),
        );
        assert!(
            latest_completion_gate_was_refused(&frame),
            "repair stays derived from the latest completion result until completion is proposed again"
        );

        frame.push_tool_result(
            completion_result(true, None),
            None,
            agent_contracts::ToolExecutionFacts::default(),
        );
        assert!(!latest_completion_gate_was_refused(&frame));
    }

    #[test]
    fn final_pack_prefers_optional_and_records_required_budget_exclusion() {
        let required = context_item(ContextRetention::Working, &"r".repeat(1_000));
        let optional = context_item(ContextRetention::Working, "optional");
        let mut materialized = MaterializedContext {
            items: vec![required.clone(), optional.clone()],
            required_item_ids: vec![required.item_id],
            ..Default::default()
        };

        assert_eq!(
            largest_final_pack_drop_index(&materialized, &materialized.items),
            Some(1),
            "optional content is displaced before a larger mandatory body"
        );
        // The runtime removes the optional copy first, then has nothing
        // left but the required body; dropping it removes the body from
        // the frame entirely and is recorded as a BudgetExcluded miss.
        let dropped_optional = materialized.items.remove(1);
        record_final_pack_drop(&mut materialized, &dropped_optional, 9);
        materialized.items.remove(0);
        record_final_pack_drop(&mut materialized, &required, 9);
        assert_eq!(materialized.required_misses.total(), 1);
        let miss = &materialized.required_misses.as_slice()[0];
        assert_eq!(miss.identity.item_id, Some(required.item_id));
        assert_eq!(miss.identity.anchor_revision, 9);
        assert_eq!(
            miss.reason,
            ContextMaterializationMissReason::BudgetExcluded
        );
        assert_eq!(materialized.optional_misses.total(), 1);
    }

    #[test]
    fn final_pack_drop_of_a_duplicate_that_stays_visible_is_not_a_miss() {
        // The same body may be present in both the selected and the
        // foreground layer; removing one copy while the other stays in the
        // final frame must not record a BudgetExcluded miss.
        let required = context_item(ContextRetention::Working, &"r".repeat(1_000));
        let mut materialized = MaterializedContext {
            items: vec![required.clone()],
            foreground: vec![required.clone()],
            required_item_ids: vec![required.item_id],
            ..Default::default()
        };
        materialized.items.remove(0);
        record_final_pack_drop(&mut materialized, &required, 9);
        assert_eq!(
            materialized.required_misses.total(),
            0,
            "the duplicate copy is still visible in the final frame"
        );
        assert_eq!(
            materialized.required_misses.total() + materialized.optional_misses.total(),
            0,
            "no miss entry may be recorded for a body that remains visible"
        );
    }

    #[test]
    fn final_required_miss_revokes_settlement_from_the_sent_arm() {
        let base = Some(TaskProgressView {
            anchor_revision: 7,
            ..TaskProgressView::default()
        });
        let (treatment, projected) = settlement_progress_views(&base, true, true, true);
        assert!(
            treatment
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_some()
        );
        assert!(
            projected
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_some()
        );

        // This is the branch taken after final packing appends a required
        // BudgetExcluded miss: the treatment probe may remain conservative,
        // but the actual provider request must return to baseline.
        let (treatment, projected) = settlement_progress_views(&base, false, true, true);
        assert!(
            treatment
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_none()
        );
        assert!(
            projected
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_none()
        );
    }

    #[test]
    fn settlement_diagnostics_are_the_only_common_envelope_and_audit_path() {
        // Ordinary product off: baseline packing and no counterfactual.
        assert!(!settlement_packing_projects(true, false, false));
        assert!(!settlement_audit_enabled(true, false));

        // Ordinary product on: pack the treatment that is actually sent,
        // without constructing the other arm or hashing it.
        assert!(settlement_packing_projects(true, true, false));
        assert!(!settlement_audit_enabled(true, false));

        // Paired diagnostics: both arms share the treatment-sized envelope
        // and only this explicit mode assembles the counterfactual audit.
        assert!(settlement_packing_projects(true, false, true));
        assert!(settlement_packing_projects(true, true, true));
        assert!(settlement_audit_enabled(true, true));
        assert!(settlement_packing_requires_counterfactual(
            true, false, true
        ));
        assert!(!settlement_packing_requires_counterfactual(
            true, false, false
        ));
        assert!(!settlement_packing_requires_counterfactual(
            true, true, true
        ));

        // No candidate means no treatment work in any mode.
        assert!(!settlement_packing_projects(false, true, true));
        assert!(!settlement_audit_enabled(false, true));
        assert!(!settlement_packing_requires_counterfactual(
            false, false, true
        ));

        let base = Some(TaskProgressView::default());
        let (diagnostic_treatment, actual) = settlement_progress_views(&base, true, false, false);
        assert!(diagnostic_treatment.is_none());
        assert!(
            actual
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_none()
        );
        let (diagnostic_treatment, actual) = settlement_progress_views(&base, true, true, false);
        assert!(diagnostic_treatment.is_none());
        assert!(
            actual
                .as_ref()
                .and_then(|progress| progress.settlement.as_ref())
                .is_some()
        );

        let ordinary = model_request_metadata(RunId::new(), 0, 0, 0, 0, None);
        assert!(
            ordinary.get("settlement_projection_audit").is_none(),
            "ordinary product requests must not even carry a null diagnostic key"
        );
        let diagnostic = model_request_metadata(
            RunId::new(),
            0,
            0,
            0,
            0,
            Some(serde_json::json!({"passed": true})),
        );
        assert_eq!(diagnostic["settlement_projection_audit"]["passed"], true);
    }

    #[test]
    fn model_failures_keep_semantic_class_and_retryability() {
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::Transport {
                retryable: true,
                message: "reset".into(),
            }),
            (RuntimeFailureClass::ProviderTransport, true)
        );
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::ModelOutputLimit {
                reason: "max_output_tokens".into(),
            }),
            (RuntimeFailureClass::ModelOutputLimit, false)
        );
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::Model("filtered".into())),
            (RuntimeFailureClass::Model, false)
        );
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::TransportRetryAfter {
                retry_after_ms: agent_contracts::RetryAfterMillis::new(250).unwrap(),
                message: "busy".into(),
            }),
            (RuntimeFailureClass::ProviderTransport, true)
        );
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::ModelProtocol {
                kind: agent_contracts::ModelProtocolErrorKind::MalformedEvent,
                message: "invalid event".into(),
            }),
            (RuntimeFailureClass::Model, false)
        );
    }
}

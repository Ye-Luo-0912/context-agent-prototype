use super::*;

use crate::task::COMPLETION_REPAIR_VIEW_CHARS;

fn is_required_context_body(materialized: &MaterializedContext, item: &MaterializedItem) -> bool {
    item.retention == ContextRetention::Pinned
        || materialized.required_item_ids.contains(&item.item_id)
}

/// T1 (R1): one trim candidate of the final packing layer. The view spans
/// every droppable partition — selected working-set bodies, foreground
/// bodies and omitable optional schemas — so required-vs-optional priority
/// is decided once against the whole frame instead of by whichever
/// partition a sequential loop happened to be draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalPackPartition {
    SelectedBody,
    ForegroundBody,
    OptionalSchema,
}

struct FinalPackCandidate {
    partition: FinalPackPartition,
    /// Index inside the source partition (`items` / `foreground`). The
    /// schema partition keeps its plan-owned omission order, so its index
    /// is always 0.
    index: usize,
    /// Identity of the candidate (item id or tool name), for diagnostics.
    #[allow(dead_code)]
    identity: String,
    required: bool,
    approx_size: usize,
}

/// Drop priority, compared with `max`: optional content anywhere before
/// required content; within the same class the partition rank keeps the
/// historical order (selected bodies, then foreground bodies, then optional
/// schemas), and the largest estimated content goes first. Ties fall back
/// to the later index, matching the previous per-list `max_by_key`
/// behaviour.
fn final_pack_candidate_key(candidate: &FinalPackCandidate) -> (u8, u8, usize, usize) {
    let rank = match candidate.partition {
        FinalPackPartition::SelectedBody => 0,
        FinalPackPartition::ForegroundBody => 1,
        FinalPackPartition::OptionalSchema => 2,
    };
    (
        u8::from(!candidate.required),
        u8::MAX - rank,
        candidate.approx_size,
        candidate.index,
    )
}

/// The next drop of the unified final-pack view. `optional_schema_candidate`
/// is the plan's own peek at what `omit_largest_for_provider_budget` would
/// remove (never a mandatory schema); bodies come from the materialized
/// frame. Returns `None` only when nothing droppable remains, after which a
/// still-overshooting request is refused, never sent.
fn largest_final_pack_candidate(
    materialized: &MaterializedContext,
    optional_schema_candidate: Option<(String, usize)>,
) -> Option<FinalPackCandidate> {
    let mut best: Option<FinalPackCandidate> = None;
    let consider = |candidate: FinalPackCandidate, best: &mut Option<FinalPackCandidate>| {
        let better = best.as_ref().is_none_or(|current| {
            final_pack_candidate_key(&candidate) > final_pack_candidate_key(current)
        });
        if better {
            *best = Some(candidate);
        }
    };
    for (index, item) in materialized.items.iter().enumerate() {
        consider(
            FinalPackCandidate {
                partition: FinalPackPartition::SelectedBody,
                index,
                identity: item.item_id.to_string(),
                required: is_required_context_body(materialized, item),
                approx_size: approx_tokens(&item.content),
            },
            &mut best,
        );
    }
    for (index, item) in materialized.foreground.iter().enumerate() {
        consider(
            FinalPackCandidate {
                partition: FinalPackPartition::ForegroundBody,
                index,
                identity: item.item_id.to_string(),
                required: is_required_context_body(materialized, item),
                approx_size: approx_tokens(&item.content),
            },
            &mut best,
        );
    }
    if let Some((name, approx_size)) = optional_schema_candidate {
        consider(
            FinalPackCandidate {
                partition: FinalPackPartition::OptionalSchema,
                index: 0,
                identity: name,
                required: false,
                approx_size,
            },
            &mut best,
        );
    }
    best
}

fn final_pack_window_covers(candidate: &MaterializedItem, dropped: &MaterializedItem) -> bool {
    // W02: a remaining copy proves the dropped body is still visible only
    // under the R09 interval rule — same path and revision, and the
    // candidate's window contains the dropped record's interval. The old
    // `path@revision` string match also let two complementary windows of
    // one revision hide each other (dropping L1–100 while L101–200 stayed
    // was counted as "still visible").
    let Some(path) = dropped
        .file_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    else {
        return false;
    };
    let Some(revision) = dropped
        .file_revision
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        // Without a revision there is no interval truth: no coverage proof.
        return false;
    };
    // Check the candidate's own identity before interpreting its range.
    // Copying the dropped identity onto an unrelated candidate would let
    // the same line numbers in any file masquerade as coverage.
    if candidate.partial_body
        || candidate.file_path.as_deref().map(str::trim) != Some(path)
        || candidate.file_revision.as_deref().map(str::trim) != Some(revision)
    {
        return false;
    }
    let covers_file = match (candidate.file_start_line, candidate.file_end_line) {
        (Some(_), Some(_)) => false,
        (None, None) => true,
        // An incomplete range cannot certify a whole-file body.
        _ => return false,
    };
    let window = agent_contracts::FileBodyWindow {
        path: path.to_string(),
        revision: Some(revision.to_string()),
        start_line: candidate.file_start_line,
        end_line: candidate.file_end_line,
        covers_file,
        // F01: the candidate here is the body that actually reached the
        // final request, so its declared range is the range the model saw.
        complete: true,
    };
    agent_contracts::visible_body_windows_cover(
        &[window],
        path,
        Some(revision),
        dropped.file_start_line,
        dropped.file_end_line,
    )
}

/// Records the budget exclusion of one removed body. `required` is the
/// classification captured BEFORE the frame was mutated (see the pack-start
/// snapshot in `continue_model_operation_after_materialize`). Returns
/// `true` when a REQUIRED miss was appended: the caller must then also drop
/// the id from `required_item_ids`, because the final materialization
/// validation demands that every still-listed required identity is
/// physically present in the frame — the miss itself now carries the
/// identity of the body that could not fit (T1/R1 honest degradation).
fn record_final_pack_drop(
    materialized: &mut MaterializedContext,
    dropped: &MaterializedItem,
    required: bool,
    active_anchor_revision: u64,
) -> bool {
    // The same body may legitimately live in both the selected and the
    // foreground layer (a resource that was both scored and explicitly
    // requested). Removing one copy is not a miss while another copy of
    // the same body stays in the final frame; recording a
    // `BudgetExcluded` entry for it would misclassify a body that remains
    // visible to the model.
    let still_visible = materialized
        .items
        .iter()
        .chain(materialized.foreground.iter())
        .any(|item| {
            // The same source may have multiple, differently clipped
            // projections. Only an exact complete projection is a copy;
            // equal bytes from another source are not attributed evidence.
            (item.item_id == dropped.item_id
                && item.content == dropped.content
                && item.file_path == dropped.file_path
                && item.file_revision == dropped.file_revision
                && item.file_start_line == dropped.file_start_line
                && item.file_end_line == dropped.file_end_line
                && !item.partial_body)
                || final_pack_window_covers(item, dropped)
        });
    if still_visible {
        return false;
    }
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
    required
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

/// EXEC-1: the prepared round locals the post-materialization tail
/// consumes. Parked with the actor while the spawned materialization
/// runs; destructured back to the original names so the tail reads
/// exactly as it did when it ran inline.
pub(super) struct ModelRoundPlan {
    pub(super) turn_id: TurnId,
    pub(super) model_round: usize,
    pub(super) turn_frame: TurnFrame,
    pub(super) runtime_focus: Option<agent_contracts::FocusState>,
    pub(super) task_view: Option<agent_contracts::TaskAnchorView>,
    pub(super) base_progress_view: Option<agent_contracts::TaskProgressView>,
    pub(super) settlement_candidate: bool,
    pub(super) project_settlement: bool,
    pub(super) settlement_projection_diagnostics: bool,
    pub(super) materialize_started: std::time::Instant,
    pub(super) output_reserve: usize,
    pub(super) send_window: usize,
    pub(super) surface_plan: RoundSurfacePlan,
    pub(super) proof_surface_available: bool,
}

/// The conservative input-token estimate of one assembled request: the wire
/// form of the final message list plus the wire form of its tool schemas.
/// The provider ingests exactly these bytes, so this is the honest measure
/// the published accounting uses.
fn assembled_input_total(input: &ModelInput) -> usize {
    approx_layer_tokens(&input.into_messages()) + approx_layer_tokens(&input.tool_schemas)
}

/// T1 (R2): the one immutable publish state of final packing. The request
/// input, its body-cache accounting and every count derived from them come
/// from the SAME assembly generation. All packing mutations — body drops,
/// schema omissions, the settlement-projection revocation — rebuild this
/// state through one recompute step, so no branch can leave a stale count
/// behind for the budget check or the Ready report.
struct FinalPackInputs {
    input: ModelInput,
    body_cache_stats: crate::prompt::ProtocolBodyAssemblyStats,
    input_total: usize,
    packing_input: Option<ModelInput>,
    packing_total: Option<usize>,
}

impl FinalPackInputs {
    /// The conservative total the final budget check prices: the diagnostic
    /// packing probe when one exists, otherwise the actual request.
    fn packed_total(&self) -> usize {
        self.packing_total.unwrap_or(self.input_total)
    }
}
impl RuntimeActor {
    /// Prepare + spawn one model round: close the consumed tool frames,
    /// maintenance, materialize, assemble, then the model call as an
    /// operation.
    pub(super) async fn spawn_model_operation(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        // EXEC-7 (R2-08): a deferred explicit collect parks here — the round
        // assembles after the pass lands, so the eviction the model asked
        // for is in place before the next decision. This is the one funnel
        // every next-model-round path goes through, including the deferred
        // resumes.
        if let Some(turn) = self.state.turn.as_mut()
            && turn.deferred_context_collect
        {
            turn.deferred_context_collect = false;
            self.begin_explicit_collect().await;
            return;
        }
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

        // Advance the decision-round counter. The
        // remaining round inputs are re-read by the maintenance continuation
        // once the fence passes.
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.model_round += 1;

        self.spawn_maintenance(maintenance::MaintenanceContinuation::BeforeModel, op_tx);
    }

    /// Round preparation after before-model maintenance landed (W04): the
    /// report is in hand, the generation fence has passed, and the tail
    /// runs to the spawned model operation exactly as the inline path did.
    pub(super) async fn continue_model_operation_after_maintenance(
        &mut self,
        op_tx: &mpsc::Sender<OperationCompletion>,
        report: AgentResult<ContextMaintenanceReport>,
    ) {
        let (turn_id, model_round, current_input, turn_frame) = {
            let Some(turn) = self.state.turn.as_ref() else {
                return;
            };
            (
                turn.turn_id,
                turn.model_round,
                turn.turn_frame.user_message.clone(),
                turn.turn_frame.clone(),
            )
        };
        let has_external_context = match report {
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
        // They come from exact task requirements, typed execution needs, the
        // current directive's explicit-load cohort, pending explicit loads
        // and the preceding model batch whose results this request will
        // consume. No free-text action plan or fixed lease duration
        // participates.
        let lease_catalog = self.services.tool_specs();
        let task_roots = self.tool_lease_roots(&lease_catalog, &[], true, true);

        // Tool lifecycle GC remains the bounded pressure/idle backstop. The
        // same source roots protect required, directive-cohort, pending-load
        // and result-delivery tools; task demand can restore schema readiness
        // but never grants authority.
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
        let turn_intent = self
            .state
            .turn
            .as_ref()
            .map(|turn| turn.turn_frame.user_message.as_str())
            .filter(|intent| !intent.is_empty());
        let completion_requested =
            turn_intent.is_some_and(crate::execution::ExecutionState::turn_requests_complete);
        let completion_repair_due = self.state.turn.as_ref().is_some_and(|turn| {
            latest_completion_gate_was_refused(&turn.turn_frame)
                || turn.execution.completion_repair.is_some()
        });
        let completion_repair_readiness = completion_repair_due
            .then(|| self.completion_readiness(CompletionIntent::ModelProposal, None));
        let completion_repair_blockers = completion_repair_readiness
            .as_ref()
            .map(CompletionReadiness::applicable_blockers)
            .unwrap_or_default();
        let completion_repair_terminal = completion_repair_readiness
            .as_ref()
            .and_then(|readiness| {
                self.state
                    .turn
                    .as_ref()
                    .and_then(|turn| turn.execution.completion_repair.as_ref())
                    .filter(|record| record.terminal_applies(readiness))
            })
            .is_some();
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
            && !completion_repair_terminal
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
                    // Surface planning must use the host-owned route: asking
                    // the model-surface resolver whether verify.run was on the
                    // previous captured surface creates a cold-start cycle in
                    // which the tool can never be loaded.
                    .and_then(|readiness| self.runtime_completion_proof_route(readiness))
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
        if completion_repair_terminal {
            surface_plan.force_completion_finalization();
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

        if !completion_repair_terminal && !unavailable_must.is_empty() {
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
            // publish a typed failed-turn terminal without fencing.
            let message =
                "the active task requires a tool that is unavailable; refusing to start the model round"
                    .to_string();
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: message.clone(),
                })
                .await;
            self.settle_failed_turn(RuntimeFailureClass::Runtime, false)
                .await;
            return;
        }

        // Compile the bounded schema profiles before anything downstream
        // consumes the plan: the assembled request, the budget checks, the
        // Ready report and the execution snapshot must all describe the
        // same final surface, so the model is never shown a tool that
        // Core's execution surface would reject. A schema-rejected
        // MustSurface requirement is an explicit unsatisfiable refusal,
        // never a silently dropped tool.
        let schema_blocked = surface_plan.compile_schema_profiles();
        if !schema_blocked.is_empty() {
            let rejected_names = schema_blocked
                .iter()
                .map(|block| block.tool_name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
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
                schema_blocked,
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
            // publish a typed failed-turn terminal without fencing.
            let message = crate::output::bound_error_message(format!(
                "the active task requires a tool whose input schema was rejected by schema compilation ({}); refusing to start the model round",
                rejected_names
            ));
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: message.clone(),
                })
                .await;
            self.settle_failed_turn(RuntimeFailureClass::Runtime, false)
                .await;
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
            // publish a typed failed-turn terminal without fencing.
            let message = format!(
                "mandatory tool schemas exceed the per-round schema budget ({} > {} tokens); refusing to start the model round",
                surface_plan.mandatory_schema_tokens(),
                MAX_TOOL_SURFACE_TOKENS
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: message.clone(),
                })
                .await;
            self.settle_failed_turn(RuntimeFailureClass::InputBudget, false)
                .await;
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
        let proof_surface_available = surface_plan
            .specs()
            .iter()
            .any(|spec| spec.name == "verify.run");
        let (runtime_focus, task_view, base_progress_view, settlement_candidate) = self
            .runtime_prompt_focus(&turn_frame, proof_surface_available)
            .await;
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
        let visible_body_windows = crate::prompt::visible_body_windows_for_request(
            &turn_frame,
            base_progress_view.as_ref(),
            &protocol_bodies,
        );
        let context_budget = model_budget.context_frame_budget;
        let query = ContextQuery {
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
                visible_body_windows,
                foreground_resources,
            },
        };
        let plan = ModelRoundPlan {
            turn_id,
            model_round,
            turn_frame,
            runtime_focus,
            task_view,
            base_progress_view,
            settlement_candidate,
            project_settlement,
            settlement_projection_diagnostics,
            materialize_started,
            output_reserve,
            send_window,
            surface_plan,
            proof_surface_available,
        };
        // EXEC-1: the engine's materialization is the round's one unbounded
        // wait (engine gate + storage reads bounded by bytes, never by
        // time). It runs as a spawned operation so Cancel/Stop keep being
        // received while it waits; the prepared tail is parked with the
        // actor and resumes from this exact plan on the operation's
        // completion. The query is fully cloned here — the actor only
        // plans, fences and commits.
        self.spawn_materialization(query, plan, op_tx);
    }

    /// EXEC-1: the round tail after the engine's materialization preview
    /// returned. Every await from here to the provider spawn is a bounded
    /// journal/event emission, so the actor's command lane stays live.
    pub(super) async fn continue_model_operation_after_materialize(
        &mut self,
        plan: ModelRoundPlan,
        result: AgentResult<MaterializedContext>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let ModelRoundPlan {
            turn_id,
            model_round,
            turn_frame,
            runtime_focus,
            task_view,
            mut base_progress_view,
            settlement_candidate,
            project_settlement,
            settlement_projection_diagnostics,
            materialize_started,
            output_reserve,
            send_window,
            mut surface_plan,
            proof_surface_available,
        } = plan;
        let materialized = match result {
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
        // Shadow Context Frame: compile the same state into the structured
        // frame manifest and emit it for measurement. This never touches
        // the model input — the assembled request below is byte-identical
        // whether or not the shadow compiler runs.
        if self.services.shadow_context_frame() {
            let manifest = crate::frame::compile_shadow_frame(&crate::frame::ShadowFrameInputs {
                run_id: self.core.run_id(),
                task_id: self.state.tasks.active(),
                anchor: task_view.as_ref(),
                materialized: &materialized,
                unresolved_ack_debts: self.state.unresolved_ack_debts.len(),
            });
            let bounded = crate::output::bound_error_message(
                serde_json::to_string(&manifest).unwrap_or_default(),
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::ContextFrameShadow { manifest: bounded })
                .await;
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
        // T1 (R1): required-body classification is snapshotted once against
        // the frame AS MATERIALIZED, so a candidate's class does not depend
        // on which copies earlier packing iterations already removed (two
        // projections of one required id stay required until one of them is
        // recorded as the miss).
        let pack_required_item_ids = materialized.required_item_ids.clone();
        let is_pack_required = |item: &MaterializedItem| {
            item.retention == ContextRetention::Pinned
                || pack_required_item_ids.contains(&item.item_id)
        };
        // T1 (R2): the packing counterfactual follows the live candidate
        // fact, and the single recompute step below re-reads it after every
        // mutation, so the counts can never outlive the projection state
        // they were derived from.
        let mut final_inputs = self.reassemble_final_pack_inputs(
            runtime_focus.as_ref(),
            task_view.as_ref(),
            progress_view.as_ref(),
            packing_progress_view.as_ref(),
            &materialized,
            &turn_frame,
            surface_plan.specs().to_vec(),
            settlement_packing_requires_counterfactual(
                settlement_candidate,
                project_settlement,
                settlement_projection_diagnostics,
            ),
        );
        // COST-3 (D04): the tracked totals are derived ONCE per assembly and
        // tracked inside `final_inputs` — the loop below never re-serializes
        // the message list outside the recompute step, and `approx_tokens`
        // remains the engine's own heuristic while the final refusal keeps
        // its conservative margin.
        let mut packed_now = final_inputs.packed_total();
        while packed_now > max_input_budget {
            // T1 (R1): ONE candidate view across selected bodies, foreground
            // bodies and omitable optional schemas. Optional content
            // anywhere goes before required content, so the partition
            // execution order can no longer sacrifice a required body while
            // another partition still holds a droppable optional candidate.
            // When only required candidates remain, the hard provider
            // budget still wins, but each removal becomes an explicit
            // `BudgetExcluded` miss — never a silent loss and never
            // fabricated coverage. A remainder whose mandatory layers still
            // overshoot is refused after the loop, never sent.
            let Some(candidate) = largest_final_pack_candidate(
                &materialized,
                surface_plan.peek_provider_budget_omission_candidate(),
            ) else {
                break;
            };
            match candidate.partition {
                FinalPackPartition::SelectedBody => {
                    let dropped = materialized.items.remove(candidate.index);
                    // S2a: whether a required miss was recorded (no covering
                    // evidence) or the obligation was picked up by a covering
                    // record, the physical record is gone from `items` — the
                    // dropped id can no longer be required to be physically
                    // present.
                    materialized
                        .required_item_ids
                        .retain(|item_id| *item_id != dropped.item_id);
                    if record_final_pack_drop(
                        &mut materialized,
                        &dropped,
                        is_pack_required(&dropped),
                        active_anchor_revision,
                    ) {
                        // A required miss was appended (no covering evidence).
                    }
                    materialized
                        .selected
                        .retain(|selection| selection.item_id != dropped.item_id);
                    materialized.approx_tokens = materialized
                        .approx_tokens
                        .saturating_sub(approx_tokens(&dropped.content));
                }
                FinalPackPartition::ForegroundBody => {
                    let dropped = materialized.foreground.remove(candidate.index);
                    // S2a: same rule as SelectedBody — the physical record
                    // is gone, so the dropped id leaves required_item_ids
                    // whether a miss was recorded or coverage satisfied the
                    // obligation.
                    materialized
                        .required_item_ids
                        .retain(|item_id| *item_id != dropped.item_id);
                    if record_final_pack_drop(
                        &mut materialized,
                        &dropped,
                        is_pack_required(&dropped),
                        active_anchor_revision,
                    ) {
                        // A required miss was appended.
                    }
                    materialized.approx_tokens = materialized
                        .approx_tokens
                        .saturating_sub(approx_tokens(&dropped.content));
                }
                FinalPackPartition::OptionalSchema => {
                    // Round-local omission of exactly the peeked candidate.
                    // Provider token pressure must never unload a catalog
                    // entry, bump its generation or make a later,
                    // larger-budget round forget the tool; the trimmed
                    // snapshot remains the one source for prompt assembly,
                    // accounting and tool-call validation in this round.
                    let omitted = surface_plan.omit_largest_for_provider_budget();
                    debug_assert!(
                        omitted.is_some(),
                        "the peered schema candidate {} vanished",
                        candidate.identity
                    );
                    let final_proof_surface_available = surface_plan
                        .specs()
                        .iter()
                        .any(|spec| spec.name == "verify.run");
                    if final_proof_surface_available != proof_surface_available {
                        self.project_completion_repair(
                            &mut base_progress_view,
                            final_proof_surface_available,
                        );
                        (treatment_progress_view, progress_view) = settlement_progress_views(
                            &base_progress_view,
                            settlement_candidate,
                            project_settlement,
                            settlement_projection_diagnostics,
                        );
                        packing_progress_view = if settlement_projection_diagnostics {
                            treatment_progress_view.clone()
                        } else {
                            progress_view.clone()
                        };
                    }
                }
            }
            final_inputs = self.reassemble_final_pack_inputs(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                packing_progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
                settlement_packing_requires_counterfactual(
                    settlement_candidate,
                    project_settlement,
                    settlement_projection_diagnostics,
                ),
            );
            packed_now = final_inputs.packed_total();
        }

        // Runtime trimming itself may have displaced a required body and
        // appended `BudgetExcluded`. Revoke the projected fact on the exact
        // request being sent. Once the candidate is revoked there is no
        // treatment exposure to compare, so a diagnostic off-arm probe is
        // dropped as well instead of retaining a second large input.
        // T1 (R2): the revoked projection changes the published request, so
        // the SAME recompute step rebuilds it together with every count —
        // the previous code left `input_total`/`packing_total` at their
        // pre-revocation values here.
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
            final_inputs = self.reassemble_final_pack_inputs(
                runtime_focus.as_ref(),
                task_view.as_ref(),
                progress_view.as_ref(),
                packing_progress_view.as_ref(),
                &materialized,
                &turn_frame,
                surface_plan.specs().to_vec(),
                settlement_packing_requires_counterfactual(
                    settlement_candidate,
                    project_settlement,
                    settlement_projection_diagnostics,
                ),
            );
        }
        // COST-3 (D04): the destructured totals ARE the final derivation —
        // no extra message re-serialization for the accounting read, and no
        // branch-local counts that can drift from the published request.
        let FinalPackInputs {
            input,
            body_cache_stats,
            input_total: estimated_input_tokens,
            packing_input,
            packing_total,
        } = final_inputs;
        let packing_input_tokens = packing_total.unwrap_or(estimated_input_tokens);

        if let Err(error) = materialized.validate_materialization() {
            self.fail_round_preparation("final_context_materialization", error)
                .await;
            return;
        }
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
            // publish the typed terminal after the budget diagnostic.
            let message = format!(
                "model input exceeds the provider window even with the context frame emptied and optional tool schemas omitted for this round ({packing_input_tokens} > {max_input_budget} conservatively packed input tokens); refusing to send"
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Failure {
                    class: RuntimeFailureClass::InputBudget,
                    retryable: false,
                    message: message.clone(),
                })
                .await;
            self.settle_failed_turn(RuntimeFailureClass::InputBudget, false)
                .await;
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
            abort: None,
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
        // A provider that is slow to observe cancellation may outlive this
        // actor. Capture only the transport lane so a stale model future
        // cannot keep tool dispatchers or workspace locks alive across
        // shutdown/recomposition.
        let model = self.services.model_transport();
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
        let mut request_metadata = model_request_metadata(
            run_id,
            materialized.selected.len(),
            materialized.approx_tokens,
            model_round,
            surface_revision,
            settlement_projection_audit,
        );
        // Application-side accounting only; this field is not prompt text
        // or a provider-specific cache-control parameter.
        request_metadata["prompt_layout"] = serde_json::json!(input.layout);
        // N04: the composition root's stable routing namespace names this
        // task on the main lane. The key is an opaque routing string —
        // stable across the rounds and restores of one task, different
        // across isolation domains — never a per-round identity or a
        // request digest. Keyless compositions keep the historical payload.
        let cache_routing = self.services.cache_routing().cloned();
        tokio::spawn(async move {
            let mut request = input.into_request(request_metadata, cancel.clone());
            if let Some(routing) = cache_routing.as_ref() {
                let task = task_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "unassigned".to_string());
                request.prompt_cache_key = Some(routing.key_for(&task, "main"));
            }
            let outcome = match model.complete_stream(request, &sink).await {
                Ok(output) => OperationOutcome::ModelOutput {
                    content: output.content,
                    tool_calls: output.tool_calls,
                    usage: output.usage,
                },
                // W4 (V7): a cancellation stays a cancellation whether or
                // not the transport wrapped it with already-settled usage —
                // matched through `failure_source` BEFORE the failure arm,
                // so the usage envelope can never reclassify it. The known
                // counters ride the outcome orthogonally; `None` keeps the
                // historical unknown semantics.
                Err(error) if matches!(error.failure_source(), AgentError::Cancelled) => {
                    OperationOutcome::Cancelled {
                        known_usage: error.reported_usage().cloned(),
                    }
                }
                Err(error) => {
                    let (class, retryable) = Self::classify_model_failure(&error);
                    OperationOutcome::Failed {
                        class,
                        retryable,
                        message: crate::output::bound_error_message(error.to_string()),
                        // COST-7 (R3-12): usage the provider already
                        // reported for the failed attempt travels with the
                        // outcome instead of degrading to unknown.
                        usage: error.reported_usage().cloned(),
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
                    maintenance: None,
                    materialization: None,
                    gc: None,
                })
                .await;
        });
    }

    fn classify_model_failure(error: &AgentError) -> (RuntimeFailureClass, bool) {
        // COST-7 (R3-12): classification looks through the usage wrapper so
        // a wrapped transport/limit failure keeps its class and retryability.
        match error.failure_source() {
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

    /// T1 (R2): the single recompute exit of final packing. Assembles the
    /// actual request input and — when the current projection mode calls
    /// for it — the diagnostic packing probe, then derives every count from
    /// those assemblies. The trimming loop and the settlement-projection
    /// revocation both go through here, so `estimated_input_tokens` and the
    /// budget check can only ever describe the request that is published.
    ///
    /// T1 抽取方案（本片完成计数单一出口；完整纯函数化按下述边界落地，
    /// 不另起第二套待办）：把「候选构建 → 统一裁剪 → 投影修订 → 最终
    /// 消息与工具表面 → 覆盖/计数/缓存计划 → 一次发布」收敛为一个纯决策
    /// 函数 `plan_final_publish(input: FinalPackDecisionInput) ->
    /// FinalPackDecision`，Actor 只保留调度、身份与提交。所需输入全部
    /// 值语义：已验证的 `MaterializedContext`、`RoundSurfacePlan` 快照、
    /// 三个投影视图（base/actual/packing）、预算（send_window、
    /// output_reserve）、`active_anchor_revision` 与组装所需的只读目录/
    /// 协议正文行。输出即 [`FinalPackInputs`] 加 `required_item_ids` 的
    /// 终态与 `proof_surface_available` 变化。两处 `&self` 依赖按现有
    /// 签名直接搬进输入结构即可纯化：`assemble_model_input`（只读
    /// assembler + 目录 + 协议正文行）与 `project_completion_repair`
    /// （只读 readiness 投影）。回归保护：现有 final-pack /
    /// settlement / input_bounds 三组反例保持红→绿等价，报告计数与
    /// 线上请求逐字节相等由
    /// `settlement_projection_revocation_republishes_counts_from_the_final_request`
    /// 钉住。
    #[allow(clippy::too_many_arguments)]
    fn reassemble_final_pack_inputs(
        &self,
        runtime_focus: Option<&FocusState>,
        task_view: Option<&TaskAnchorView>,
        progress_view: Option<&TaskProgressView>,
        packing_progress_view: Option<&TaskProgressView>,
        materialized: &MaterializedContext,
        turn_frame: &TurnFrame,
        tool_specs: Vec<ToolSpec>,
        assemble_counterfactual: bool,
    ) -> FinalPackInputs {
        let (input, body_cache_stats) = self.assemble_model_input(
            runtime_focus,
            task_view,
            progress_view,
            materialized,
            turn_frame,
            tool_specs.clone(),
        );
        let input_total = assembled_input_total(&input);
        let packing_input = assemble_counterfactual.then(|| {
            self.assemble_model_input(
                runtime_focus,
                task_view,
                packing_progress_view,
                materialized,
                turn_frame,
                tool_specs,
            )
            .0
        });
        let packing_total = packing_input.as_ref().map(assembled_input_total);
        FinalPackInputs {
            input,
            body_cache_stats,
            input_total,
            packing_input,
            packing_total,
        }
    }

    /// Re-injectable protocol bodies with their real exposed windows. The
    /// rows are the assembler's own row type, so the coverage each row can
    /// prove travels with it instead of being re-derived (F01).
    fn eligible_protocol_bodies(&self) -> Vec<crate::prompt::ProtocolBodyRow> {
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
            turn_intent: self
                .state
                .turn
                .as_ref()
                .map(|turn| turn.turn_frame.user_message.as_str())
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
        if let Some(turn) = self.state.turn.as_ref() {
            // Unresolved obligations keep their exact source tool surfaced
            // independently of the short-lived load/result lease switch:
            // the ledger recorded a trusted association when the row opened,
            // and this derived view releases it exactly when the row dies.
            let obligation_source_tools = turn.execution.obligation_source_tools();
            roots.extend(obligation_source_tools.iter().cloned());
            // Explicit loads are a bounded continuity source for the active
            // directive. Unlike the one-decision result lease, this root must
            // also participate in the reconciliation call that deliberately
            // excludes short-lived turn leases; otherwise an intervening
            // fs.read/edit would evict a capability the model explicitly
            // loaded for the current work loop.
            roots.extend(
                turn.directive_loaded_tools
                    .iter()
                    .take(MAX_DIRECTIVE_LOADED_TOOLS)
                    .cloned(),
            );
            if include_turn_leases {
                roots.extend(turn.pending_loaded_tools.iter().cloned());
                roots.extend(turn.result_delivery_tools.iter().cloned());
            }
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
    /// other at adjacent decisions. The directive cohort additionally keeps
    /// explicitly loaded tools available across non-empty decisions; an empty
    /// decision ends the turn and releases that cohort plus every unused
    /// pending load. Reconciliation happens before dispatch, while the actor
    /// is at a surface-safe boundary.
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
        // An empty decision is the terminal edge of this directive. Release
        // the continuity cohort before computing roots so the catalog can
        // cool explicitly loaded optional schemas at the same safe point;
        // non-empty decisions keep the cohort alive for the next action.
        if calls.is_empty()
            && let Some(turn) = self.state.turn.as_mut()
        {
            turn.directive_loaded_tools.clear();
        }
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
        proof_surface_available: bool,
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
        self.project_completion_repair(&mut progress, proof_surface_available);
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

    /// Completion repair is a Runtime liveness layer, not the optional
    /// TaskProgress experiment. It therefore creates a minimal bounded frame
    /// when needed and always renders against the tool surface that will
    /// actually reach the provider.
    fn project_completion_repair(
        &self,
        progress: &mut Option<TaskProgressView>,
        proof_surface_available: bool,
    ) {
        let due = self.state.turn.as_ref().is_some_and(|turn| {
            latest_completion_gate_was_refused(&turn.turn_frame)
                || turn.execution.completion_repair.is_some()
        });
        if !due {
            return;
        }
        let readiness = self.completion_readiness(CompletionIntent::ModelProposal, None);
        let episode = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.execution.completion_repair.as_ref())
            .filter(|record| record.matches_episode(&readiness));
        // Persist only typed episode facts as authority. Plan/text in old
        // checkpoints are diagnostic compatibility fields and are never
        // injected into the prompt.
        let (_, rendered) = self.completion_repair_plan(
            &readiness,
            episode.map(|record| record.refusal_count).unwrap_or(0),
            episode.map(|record| record.no_progress_steps).unwrap_or(0),
            episode.is_some_and(|record| record.terminal_applies(&readiness)),
            proof_surface_available,
        );
        if progress.is_none() {
            let source = self
                .state
                .turn
                .as_ref()
                .map(|turn| turn.execution.view())
                .or_else(|| {
                    self.state
                        .task_id
                        .and_then(|task_id| self.state.tasks.get(task_id))
                        .map(|task| task.resume.view())
                })
                .unwrap_or_default();
            *progress = Some(TaskProgressView {
                anchor_revision: source.anchor_revision,
                workspace_revision: source.workspace_revision,
                failed_commands: source.failed_commands,
                unresolved_blockers: source.unresolved_blockers,
                completion_commit_failure: source.completion_commit_failure,
                ..TaskProgressView::default()
            });
        }
        progress
            .as_mut()
            .expect("repair projection exists")
            .completion_repair = Some(bounded_preview(&rendered, COMPLETION_REPAIR_VIEW_CHARS));
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
            file_start_line: None,
            file_end_line: None,
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

        let candidate = largest_final_pack_candidate(&materialized, None)
            .expect("a droppable candidate exists");
        assert_eq!(
            candidate.partition,
            FinalPackPartition::SelectedBody,
            "optional content is displaced before a larger mandatory body"
        );
        assert_eq!(candidate.index, 1);
        assert!(!candidate.required);
        // The runtime removes the optional copy first, then has nothing
        // left but the required body; dropping it removes the body from
        // the frame entirely and is recorded as a BudgetExcluded miss.
        let dropped_optional = materialized.items.remove(1);
        assert!(!record_final_pack_drop(
            &mut materialized,
            &dropped_optional,
            false,
            9,
        ));
        materialized.items.remove(0);
        assert!(record_final_pack_drop(
            &mut materialized,
            &required,
            true,
            9
        ));
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
    fn final_pack_unified_candidate_keeps_required_body_while_foreground_is_optional() {
        // R1 counterexample at the selector level: the selected layer holds
        // only a small REQUIRED body while the foreground layer holds an
        // OPTIONAL one. The unified view must pick the optional foreground
        // body no matter the partition order; the old per-list selector
        // fell back to the required body because its list had no optional
        // candidate left.
        let required = context_item(ContextRetention::Working, &"r".repeat(1_000));
        let required_id = required.item_id;
        let optional_foreground = context_item(ContextRetention::Working, "foreground");
        let materialized = MaterializedContext {
            items: vec![required],
            foreground: vec![optional_foreground],
            required_item_ids: vec![required_id],
            ..Default::default()
        };
        let candidate =
            largest_final_pack_candidate(&materialized, None).expect("candidate exists");
        assert_eq!(candidate.partition, FinalPackPartition::ForegroundBody);
        assert!(!candidate.required, "optional foreground must win");
    }

    #[test]
    fn final_pack_unified_candidate_prefers_optional_schema_over_required_body() {
        let required = context_item(ContextRetention::Working, &"r".repeat(5_000));
        let materialized = MaterializedContext {
            items: vec![required.clone()],
            required_item_ids: vec![required.item_id],
            ..Default::default()
        };
        let candidate =
            largest_final_pack_candidate(&materialized, Some(("optional.large".to_string(), 64)))
                .expect("candidate exists");
        assert_eq!(
            candidate.partition,
            FinalPackPartition::OptionalSchema,
            "a round-local optional schema goes before a required body"
        );
        assert_eq!(candidate.identity, "optional.large");
    }

    #[test]
    fn final_pack_unified_candidate_orders_optional_bodies_before_optional_schema() {
        let optional_body = context_item(ContextRetention::Working, &"o".repeat(10));
        let materialized = MaterializedContext {
            items: vec![optional_body],
            ..Default::default()
        };
        let candidate = largest_final_pack_candidate(
            &materialized,
            Some(("optional.large".to_string(), 9_000)),
        )
        .expect("candidate exists");
        assert_eq!(
            candidate.partition,
            FinalPackPartition::SelectedBody,
            "the historical optional-kind order (bodies, then schemas) is preserved"
        );
    }

    #[test]
    fn final_pack_unified_candidate_falls_back_to_the_largest_required_body() {
        let small = context_item(ContextRetention::Working, "s");
        let large = context_item(ContextRetention::Working, &"l".repeat(500));
        let materialized = MaterializedContext {
            items: vec![small.clone(), large.clone()],
            required_item_ids: vec![small.item_id, large.item_id],
            ..Default::default()
        };
        let candidate = largest_final_pack_candidate(&materialized, None).expect("candidate");
        assert_eq!(candidate.partition, FinalPackPartition::SelectedBody);
        assert!(candidate.required);
        assert_eq!(
            candidate.identity,
            large.item_id.to_string(),
            "with nothing optional left, the largest required body is displaced first"
        );
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
        assert!(!record_final_pack_drop(
            &mut materialized,
            &required,
            true,
            9
        ));
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

    fn windowed_body(
        retention: ContextRetention,
        content: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> MaterializedItem {
        let mut item = context_item(retention, content);
        item.file_path = Some("src/main.rs".into());
        item.file_revision = Some("rev-1".into());
        item.file_start_line = start_line;
        item.file_end_line = end_line;
        item
    }

    #[test]
    fn final_pack_coverage_requires_candidate_identity_and_complete_body() {
        let required = windowed_body(
            ContextRetention::Working,
            &"required".repeat(50),
            Some(10),
            Some(20),
        );
        let mut covering = windowed_body(
            ContextRetention::Working,
            &"covering".repeat(60),
            Some(1),
            Some(30),
        );
        let mut cases = Vec::new();
        covering.file_path = Some("src/other.rs".into());
        cases.push(("different path", covering.clone()));
        covering.file_path = required.file_path.clone();
        covering.file_revision = Some("rev-2".into());
        cases.push(("different revision", covering.clone()));
        covering.file_revision = None;
        cases.push(("unknown revision", covering.clone()));
        covering.file_revision = required.file_revision.clone();
        covering.file_end_line = None;
        cases.push(("incomplete range", covering));
        let mut clipped = required.clone();
        clipped.content.truncate(8);
        clipped.partial_body = true;
        cases.push(("same id with clipped body", clipped));
        let mut different_window = required.clone();
        different_window.content = "another part of the same source".into();
        different_window.file_start_line = Some(21);
        different_window.file_end_line = Some(30);
        cases.push(("same id with complementary range", different_window));
        let mut coincidental_text = required.clone();
        coincidental_text.item_id = agent_contracts::ContextItemId::new();
        coincidental_text.file_path = Some("src/other.rs".into());
        cases.push(("identical text from another file", coincidental_text));

        for (case, candidate) in cases {
            let mut materialized = MaterializedContext {
                items: vec![candidate],
                required_item_ids: vec![required.item_id],
                ..Default::default()
            };
            record_final_pack_drop(&mut materialized, &required, true, 9);
            assert_eq!(materialized.required_misses.total(), 1, "{case}");
        }

        let covering = windowed_body(
            ContextRetention::Working,
            "complete larger window of the same revision",
            Some(1),
            Some(30),
        );
        let mut materialized = MaterializedContext {
            foreground: vec![covering],
            required_item_ids: vec![required.item_id],
            ..Default::default()
        };
        assert!(!record_final_pack_drop(
            &mut materialized,
            &required,
            true,
            9
        ));
        assert!(materialized.required_misses.is_empty());
    }

    #[test]
    fn final_pack_complementary_windows_of_one_revision_record_the_required_miss() {
        // W02 counter-example: two required bodies of one file/revision
        // with complementary windows. Budget drops the larger (L1–100)
        // while the smaller (L101–200) remains: the old `path@revision`
        // string match counted the drop as still visible, so
        // required_body_present=false coexisted with zero required misses.
        let large = windowed_body(
            ContextRetention::Working,
            &"r".repeat(1_000),
            Some(1),
            Some(100),
        );
        let small = windowed_body(
            ContextRetention::Working,
            "s".repeat(120).as_str(),
            Some(101),
            Some(200),
        );
        let mut materialized = MaterializedContext {
            items: vec![large.clone(), small.clone()],
            required_item_ids: vec![large.item_id, small.item_id],
            ..Default::default()
        };
        let dropped = materialized.items.remove(0);
        assert!(record_final_pack_drop(&mut materialized, &dropped, true, 9));
        assert_eq!(
            materialized.required_misses.total(),
            1,
            "the L1–100 window is gone and L101–200 cannot cover it"
        );
        assert_eq!(
            materialized.required_misses.as_slice()[0].identity.item_id,
            Some(large.item_id)
        );

        // The mirror case stays correct: dropping the smaller window while
        // the whole-body copy remains is not a miss.
        let whole = windowed_body(
            ContextRetention::Working,
            "w".repeat(200).as_str(),
            None,
            None,
        );
        let tail = windowed_body(
            ContextRetention::Working,
            "s".repeat(120).as_str(),
            Some(101),
            Some(200),
        );
        let mut materialized = MaterializedContext {
            items: vec![whole.clone(), tail],
            required_item_ids: vec![whole.item_id],
            ..Default::default()
        };
        let dropped = materialized.items.pop().unwrap();
        assert!(!record_final_pack_drop(
            &mut materialized,
            &dropped,
            true,
            9
        ));
        assert_eq!(
            materialized.required_misses.total(),
            0,
            "the whole-body copy of the same revision still covers L101–200"
        );
    }

    #[test]
    fn final_pack_partial_copy_does_not_cover_and_identical_text_does() {
        // A clipped copy (partial_body) must not stand in for the dropped
        // body even when its declared window is wide enough.
        let real = windowed_body(
            ContextRetention::Working,
            &"r".repeat(500),
            Some(1),
            Some(100),
        );
        let mut clipped = windowed_body(
            ContextRetention::Working,
            &"r".repeat(80),
            Some(1),
            Some(100),
        );
        clipped.partial_body = true;
        let mut materialized = MaterializedContext {
            items: vec![real.clone(), clipped],
            required_item_ids: vec![real.item_id],
            ..Default::default()
        };
        let dropped = materialized.items.remove(0);
        assert!(record_final_pack_drop(&mut materialized, &dropped, true, 9));
        assert_eq!(
            materialized.required_misses.total(),
            1,
            "a clipped copy is no coverage proof"
        );

        // Byte-identical text elsewhere in the frame is genuine visibility.
        let body = windowed_body(
            ContextRetention::Working,
            &"r".repeat(500),
            Some(1),
            Some(100),
        );
        let twin = windowed_body(
            ContextRetention::Working,
            &"r".repeat(500),
            Some(1),
            Some(100),
        );
        let mut materialized = MaterializedContext {
            items: vec![body.clone(), twin],
            required_item_ids: vec![body.item_id],
            ..Default::default()
        };
        let dropped = materialized.items.remove(0);
        assert!(!record_final_pack_drop(
            &mut materialized,
            &dropped,
            true,
            9
        ));
        assert_eq!(
            materialized.required_misses.total(),
            0,
            "an identical copy still shows the same text to the model"
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
        assert_eq!(
            RuntimeActor::classify_model_failure(&AgentError::LocalResourceLimit {
                kind: agent_contracts::LocalResourceLimitKind::BufferedModelStreamChunks,
                observed: 16_385,
                limit: 16_384,
            }),
            (RuntimeFailureClass::Runtime, false),
            "local buffering pressure is neither provider damage nor retryable"
        );
    }
}

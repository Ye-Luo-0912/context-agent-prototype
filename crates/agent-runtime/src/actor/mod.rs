//! The runtime actor: owns the mutable runtime state and drives the turn
//! execution state machine. Model rounds and tool calls are *operations*:
//! they execute as spawned tasks and report an `OperationResult`; the actor
//! validates the result against the current generation and only then commits
//! it (context ingest/maintenance, turn-frame pushes, events). Stale results
//! are dropped — they never race into the new state.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentError, AgentResult, AnchorPatchKind, ArgumentDigest, ArtifactLocator, AuthorityLease,
    AuthorityRecoveryStatus, CAPABILITY_MANAGE, CONTEXT_CONSUMPTION_ACK_ITEM_CAP, CONTEXT_MANAGE,
    CancellationToken, CompletionOpportunityDisposition, CompletionProposal, ContextConsumptionAck,
    ContextHints, ContextIngress, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextRetention, DISCOVERY_IDENTICAL_QUERY_BUDGET,
    DISCOVERY_MAX_QUERIES_PER_TURN, DiscoveryBudgetExhausted, DiscoveryTurnBudget, Effect,
    EffectDurability, EffectId, EffectReceipt, FocusState, FsRereadClass, InputAuthority,
    InputKind, InputLifecycle, InputSource, MAX_COMPLETION_ARTIFACTS,
    MAX_MODEL_TOOL_CALLS_PER_ROUND, MaterializedContext, ModelCompletionValidity, ModelInput,
    ModelRequest, OperationId, OperationOutcome, OperationQueryResult, OperationResult,
    OperationState, OperationTerminal, ResourceFreshness, ResourceKey, ResourceVersionOracle,
    RestoreRevision, RunId, RuntimeDirective, RuntimeEvent, RuntimeFailureClass,
    RuntimeInputEnvelope, RuntimeInputId, ScopeId, ScopeKind, StatePatchProposal, TaskAnchorView,
    TaskId, TaskProgressProposal, TaskProgressView, ToolCall, ToolLeaseBoundary,
    ToolOperationIdentity, ToolOutcome, ToolOutput, ToolResultDisposition, ToolSpec,
    ToolSurfaceBlock, ToolSurfaceBlockReason, ToolSurfaceDemand, ToolSurfaceRequirement,
    ToolSurfaceSnapshot, TurnCancelAck, TurnCancellationReason, TurnFrame, TurnFrameStep, TurnId,
    USER_INPUT_ARTIFACT_OWNER, USER_INPUT_PREVIEW_CHARS, USER_INPUT_QUEUE_CAP,
    apply_runtime_diagnosis, bounded_preview, context_maintenance_events,
    discovery_search_from_call, path_exactly_in_directive,
};
use agent_core::{
    ApprovalVerdict, CorePort, EffectCommitDisposition, EffectCommitRejection, EffectCommitRequest,
    EffectRollbackRequest, OperationCancelDisposition, ToolOperationAdmission,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::budget::{
    DEFAULT_OUTPUT_RESERVE, MAX_TOOL_SURFACE_TOKENS, ModelBudget, approx_layer_tokens,
    engine_pack_window, provider_send_window,
};
use crate::checkpoint::RuntimeCheckpoint;
use crate::command::{Reply, RuntimeCommand, RuntimeHandle};
use crate::execution::{ExecutionState, RoundExecutionSnapshot, RuntimeExecutionAttribution};
use crate::output::bound_tool_output;
use crate::prompt::PromptAssembler;
use crate::services::RuntimeServices;
use crate::sink::LiveSink;
use crate::surface::{RoundSurfacePlan, SurfaceReportContext};
use crate::task::{
    AnchorPatch, TaskManager, changed_fields_kind, completion_from_execution,
    normalize_tool_requirements, validate_completion_proposal,
};

mod commands;
mod lifecycle;
mod model;
mod restore;
#[cfg(test)]
mod restore_tests;
mod safepoint;
mod tools;
mod turn;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// `capability.manage` search/inspect are read-only catalog views; they must
/// not become ToolObservations. load/unload still persist.
fn tool_value_disposition(output: &ToolOutput) -> ToolResultDisposition {
    if output.tool_name != CAPABILITY_MANAGE {
        return ToolResultDisposition::PersistObservation;
    }
    match output.metadata.get("op").and_then(|v| v.as_str()) {
        Some("search" | "inspect") => ToolResultDisposition::TransientNoPersist,
        _ => ToolResultDisposition::PersistObservation,
    }
}

fn discovery_budget_refusal(call: &ToolCall, exhausted: DiscoveryBudgetExhausted) -> ToolOutput {
    let (summary, content) = match exhausted {
        DiscoveryBudgetExhausted::QueryCount => (
            "discovery search refused: per-turn query budget exhausted",
            format!(
                "at most {DISCOVERY_MAX_QUERIES_PER_TURN} discovery searches are allowed in one user turn"
            ),
        ),
        DiscoveryBudgetExhausted::IdenticalQuery => (
            "discovery search refused: identical-query budget exhausted",
            format!(
                "the same discovery search may run at most {DISCOVERY_IDENTICAL_QUERY_BUDGET} times in one user turn"
            ),
        ),
    };
    ToolOutput {
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        ok: false,
        summary: summary.into(),
        model_content: content,
        artifact_ref: None,
        metadata: serde_json::json!({
            "code": exhausted.code(),
            "executed": false,
            "op": "search",
        }),
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn output(name: &str, op: &str) -> ToolOutput {
        ToolOutput {
            call_id: "c1".into(),
            tool_name: name.into(),
            ok: true,
            summary: "ok".into(),
            model_content: "ok".into(),
            artifact_ref: None,
            metadata: serde_json::json!({ "op": op }),
        }
    }

    #[test]
    fn capability_search_and_inspect_are_transient() {
        assert_eq!(
            tool_value_disposition(&output(CAPABILITY_MANAGE, "search")),
            ToolResultDisposition::TransientNoPersist
        );
        assert_eq!(
            tool_value_disposition(&output(CAPABILITY_MANAGE, "inspect")),
            ToolResultDisposition::TransientNoPersist
        );
        assert_eq!(
            tool_value_disposition(&output(CAPABILITY_MANAGE, "load")),
            ToolResultDisposition::PersistObservation
        );
        assert_eq!(
            tool_value_disposition(&output("fs.read", "search")),
            ToolResultDisposition::PersistObservation
        );
    }
}

/// Which operation a spawned task is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Model,
    Tool,
}

const TOOL_SCOPE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_SCOPE_CLOSE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_OPERATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Identity of one in-flight operation, captured when it is spawned so its
/// late completion can be validated before any commit.
struct InFlightOp {
    operation_id: OperationId,
    turn_id: TurnId,
    generation: u64,
    kind: OpKind,
    /// The tool scope opened for this operation (tool ops only).
    scope_id: Option<ScopeId>,
    /// Full tool-operation identity lets cancellation reserve the exact
    /// terminal in Core even if it races ahead of Core's normal admission.
    /// Model operations do not use the tool-operation registry.
    tool_identity: Option<ToolOperationIdentity>,
    cancel: CancellationToken,
}

/// Bounded retries of a structurally empty provider completion (empty
/// content, no tool calls, 0/0 usage) when this turn has no persistable
/// tool delta yet. The first empty response plus this many retries.
const MAX_STRUCTURALLY_EMPTY_RETRIES: u8 = 2;

/// One trusted topology-recovery surface request derived from a typed tool
/// refusal. Body-free and hard-bounded; only the runtime's typed recovery
/// pipeline constructs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecoverySurfaceRequest {
    /// The exact host-owned tool to prefer for the next decision.
    pub(crate) tool_name: String,
    /// The typed recovery target (e.g. the first missing directory), used
    /// for audit and deduplication only.
    pub(crate) target: String,
}

/// Pure derivation of a trusted topology-recovery surface request from one
/// terminal tool result. Only a typed `parent_path_not_found` whose
/// `next_directory` names the first creatable component qualifies: the
/// recovery contract requires topology mutation, so the next decision gets
/// the host-owned `fs.mkdir`. Unrelated missing reads (no `next_directory`)
/// and non-mutating producers never qualify.
pub(super) fn derive_recovery_surface_request(
    output: &agent_contracts::ToolOutput,
) -> Option<RecoverySurfaceRequest> {
    if output.ok {
        return None;
    }
    let target = output
        .metadata
        .get("next_directory")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())?;
    if !output.may_mutate_workspace() {
        return None;
    }
    let mut target = target.to_string();
    if target.chars().count() > 512 {
        target = target.chars().take(512).collect();
    }
    Some(RecoverySurfaceRequest {
        tool_name: "fs.mkdir".into(),
        target,
    })
}

#[cfg(test)]
mod recovery_surface_tests {
    use super::*;

    fn failed_write_output(next_directory: Option<&str>) -> ToolOutput {
        let mut metadata = serde_json::json!({});
        if let Some(dir) = next_directory {
            metadata["next_directory"] = serde_json::json!(dir);
        }
        ToolOutput {
            call_id: "c1".into(),
            tool_name: "fs.write".into(),
            ok: false,
            summary: "fs.write refused: parent_path_not_found".into(),
            model_content: "parent directory does not exist".into(),
            artifact_ref: None,
            metadata,
        }
    }

    #[test]
    fn failed_mutating_output_with_next_directory_arms_request() {
        let request = derive_recovery_surface_request(&failed_write_output(Some("a/b")));
        let request = request.expect("typed recovery target must arm the request");
        assert_eq!(request.tool_name, "fs.mkdir");
        assert_eq!(request.target, "a/b");
    }

    #[test]
    fn ok_output_never_arms_request() {
        let ok = ToolOutput {
            call_id: "c1".into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: "written".into(),
            model_content: "ok".into(),
            artifact_ref: None,
            metadata: serde_json::json!({"next_directory": "a/b"}),
        };
        assert!(derive_recovery_surface_request(&ok).is_none());
    }

    #[test]
    fn missing_or_empty_next_directory_never_arms_request() {
        assert!(derive_recovery_surface_request(&failed_write_output(None)).is_none());
        assert!(derive_recovery_surface_request(&failed_write_output(Some(""))).is_none());
    }

    #[test]
    fn observation_missing_read_without_next_directory_never_arms_request() {
        // A `path_not_found` from a read carries no `next_directory`; the
        // recovery contract there is observation-only, not topology mutation.
        let read = ToolOutput {
            call_id: "c1".into(),
            tool_name: "fs.read".into(),
            ok: false,
            summary: "fs.read refused: path_not_found".into(),
            model_content: "no such file".into(),
            artifact_ref: None,
            metadata: serde_json::json!({"failure_class": "PathNotFound"}),
        };
        assert!(derive_recovery_surface_request(&read).is_none());
    }

    #[test]
    fn non_mutating_producer_never_arms_request() {
        // A search tool with a typed `next_directory` metadata key must not
        // qualify: only workspace-mutating producers may prove the recovery
        // contract requires topology mutation.
        let search = ToolOutput {
            call_id: "c1".into(),
            tool_name: "search.grep".into(),
            ok: false,
            summary: "search failed".into(),
            model_content: "no match".into(),
            artifact_ref: None,
            metadata: serde_json::json!({"next_directory": "a/b"}),
        };
        assert!(derive_recovery_surface_request(&search).is_none());
    }

    #[test]
    fn target_is_truncated_to_bounded_length() {
        let long = "长".repeat(1000);
        let request = derive_recovery_surface_request(&failed_write_output(Some(&long)))
            .expect("a long but valid target must still arm the request");
        assert_eq!(request.target.chars().count(), 512);
    }
}

/// The runtime's view of the active turn: the execution stack (TurnFrame),
/// how many model rounds ran, tool calls still waiting to run, and the
/// One turn's mutable state: the model round counter, the tool calls the
/// model queued for execution, the in-flight operation, and the tool
/// surface snapshot captured at the round start — budget, prompt and
/// tool-call validation all share it.
struct ActiveTurn {
    turn_id: TurnId,
    turn_frame: TurnFrame,
    model_round: usize,
    pending_tools: VecDeque<ToolCall>,
    /// Optional tools explicitly loaded by the model but not yet called in
    /// this directive. Unlike a time-to-live lease, this set advances on a
    /// semantic event: using the exact tool consumes its pending-load root.
    /// Sibling loads therefore remain available while the model assembles a
    /// small task-specific tool cohort. The set is unique, turn-scoped and
    /// never checkpointed or copied into Context.
    pending_loaded_tools: Vec<String>,
    /// Exact tools selected by the last successful model decision. Their
    /// schemas stay rooted while the calls run and until the following model
    /// decision consumes their results. The following decision either renews
    /// the lease by calling the tool again or releases it. Bounded by the
    /// model tool-call batch contract; never checkpointed across directives.
    result_delivery_tools: Vec<String>,
    /// Current model-requested action batch. It is bounded by
    /// `MAX_MODEL_TOOL_CALLS_PER_ROUND`, contains counters only, and never
    /// enters Context, prompt history or a checkpoint.
    action_batch: Option<TurnActionBatch>,
    /// The tool surface of the current model round, captured once after the
    /// tool lifecycle GC. `None` only before the first round starts.
    tool_surface: Option<ToolSurfaceSnapshot>,
    /// Where the turn is in its commit lifecycle (see `TurnState`).
    turn_state: TurnState,
    op: Option<InFlightOp>,
    /// Ephemeral execution projection for this turn. Seeded from
    /// `TaskRecord.resume` at turn start; observed on persistable tool
    /// results; installed back onto the task only after TurnCompleted.
    /// Cancel / fail / stale drops this value.
    execution: ExecutionState,
    /// deterministically-refused edit attempts this turn.
    /// An identical retry (same argument digest) against unchanged file
    /// identities is refused without dispatch. Turn-scoped on purpose:
    /// a new user directive may legitimately ask for the same edit.
    edit_attempts: Vec<EditAttemptFact>,
    /// 本轮的程序解析失败记录（可证等价重试域）。同轮内
    /// 同参数重试在版本未推进时被无派发拒绝。
    launch_failures: Vec<LaunchResolutionFact>,
    /// 本轮协议正文缓存。只在组装下一轮请求时按严格
    /// 条件回注；不进 Context、不被 admit、不落盘。
    protocol_bodies: crate::execution::body_cache::ProtocolBodyCache,
    /// Captured once per BeforeModel after revalidate. Prompt, ContextHints,
    /// and tool-surface policy all read this — not per-consumer clones.
    round_snapshot: Option<RoundExecutionSnapshot>,
    /// A structured completion proposal the model attached to a tool call
    /// (`task.complete`). Committed at the turn's safe point — after the
    /// turn commits — through the CTX-10 transaction, so completion never
    /// races an in-flight operation.
    pending_completion: Option<CompletionProposal>,
    /// LONG-TASK advisory (Slice C): opportunity key whose one-decision
    /// lease is outstanding — `task.complete` is preferred on this
    /// decision's surface and the prompt carries the bounded statement.
    /// Dies with the turn on cancel/fail (retraction); cleared after one
    /// decision regardless of outcome.
    opportunity_lease: Option<String>,
    /// Trusted runtime-derived recovery source for the NEXT decision only:
    /// a typed `parent_path_not_found` proved topology mutation is the
    /// recovery, so the host-owned `fs.mkdir` schema is preferred exactly
    /// once, then cleared. Model calls cannot create this state; unrelated
    /// missing reads never touch it.
    recovery_surface_request: Option<RecoverySurfaceRequest>,
    /// 本轮 Applied 对话。Consumed / Archived / InterruptCommitted 复用同一条。
    applied_input: Option<RuntimeInputEnvelope>,
    /// 已经为这条 applied input 发过 Consumed。
    input_consumed: bool,
    /// Provider 0/0 empty completions retried this turn. Not checkpointed.
    structurally_empty_retries: u8,
    /// Tool scopes whose results have landed and still need their context
    /// close. Enqueued exactly once when a result settles; drained at the
    /// next round boundary or on cancellation. Replaces rescanning the
    /// whole turn frame every round (O(R²) with repeated no-op closes
    /// inflating `event_seq`).
    pending_scope_closes: VecDeque<agent_contracts::ScopeId>,
}

#[derive(Debug, Clone, Copy)]
enum ActionDispatch {
    Spawned,
    Refused,
    Reused,
}

/// Actor-local, body-free accounting for one model tool-call batch.
#[derive(Debug, Clone)]
struct TurnActionBatch {
    model_round: usize,
    requested: usize,
    terminal: usize,
    spawned: usize,
    refused: usize,
    reused: usize,
    persist_observation: usize,
    transient_no_persist: usize,
    access_event_only: usize,
    succeeded: usize,
    failed: usize,
    known_mutation_results: usize,
    typed_verification_results: usize,
    unknown_invalidations: usize,
    completion_proposals: usize,
    outcome_advances: usize,
    no_outcome_results: usize,
}

impl TurnActionBatch {
    fn new(model_round: usize, requested: usize) -> Self {
        Self {
            model_round,
            requested,
            terminal: 0,
            spawned: 0,
            refused: 0,
            reused: 0,
            persist_observation: 0,
            transient_no_persist: 0,
            access_event_only: 0,
            succeeded: 0,
            failed: 0,
            known_mutation_results: 0,
            typed_verification_results: 0,
            unknown_invalidations: 0,
            completion_proposals: 0,
            outcome_advances: 0,
            no_outcome_results: 0,
        }
    }

    /// A provider batch rejected as one bounded protocol unit before any
    /// individual call is admitted. Each requested call receives the same
    /// terminal no-dispatch refusal so `requested == terminal` still holds;
    /// no call body, arguments or synthetic ToolOutput is retained.
    fn refused_before_dispatch(model_round: usize, requested: usize) -> Self {
        let mut batch = Self::new(model_round, requested);
        batch.terminal = requested;
        batch.refused = requested;
        batch.transient_no_persist = requested;
        batch.failed = requested;
        batch.no_outcome_results = requested;
        batch
    }

    fn record(
        &mut self,
        output: &ToolOutput,
        disposition: ToolResultDisposition,
        mut dispatch: ActionDispatch,
        trusted_verification: Option<bool>,
        facts: &agent_contracts::ToolExecutionFacts,
    ) {
        self.terminal = self.terminal.saturating_add(1);
        if matches!(dispatch, ActionDispatch::Spawned)
            && output
                .metadata
                .get("executed")
                .and_then(|value| value.as_bool())
                == Some(false)
        {
            dispatch = ActionDispatch::Refused;
        }
        match dispatch {
            ActionDispatch::Spawned => self.spawned = self.spawned.saturating_add(1),
            ActionDispatch::Refused => self.refused = self.refused.saturating_add(1),
            ActionDispatch::Reused => self.reused = self.reused.saturating_add(1),
        }
        match disposition {
            ToolResultDisposition::PersistObservation => {
                self.persist_observation = self.persist_observation.saturating_add(1)
            }
            ToolResultDisposition::TransientNoPersist => {
                self.transient_no_persist = self.transient_no_persist.saturating_add(1)
            }
            ToolResultDisposition::AccessEventOnly => {
                self.access_event_only = self.access_event_only.saturating_add(1)
            }
        }
        if output.ok {
            self.succeeded = self.succeeded.saturating_add(1);
        } else {
            self.failed = self.failed.saturating_add(1);
        }

        // the ledger consumes typed facts from the dispatcher
        // lane. For untrusted producers the facts are empty and the
        // mutation bound falls back exactly like the legacy accessor; for
        // the trusted builtin host the stamps arrive pre-translated.
        let footprint = facts.mutation_footprint(output.ok, output.may_mutate_workspace());
        let known_mutation = output.ok
            && matches!(
                &footprint,
                agent_contracts::MutationFootprint::Known(touches) if !touches.is_empty()
            );
        let verification = trusted_verification
            .or(facts.is_verification())
            .unwrap_or(false);
        let unknown = matches!(footprint, agent_contracts::MutationFootprint::Unknown);
        let completion = output.ok && output.tool_name == "task.complete";
        self.known_mutation_results = self
            .known_mutation_results
            .saturating_add(usize::from(known_mutation));
        self.typed_verification_results = self
            .typed_verification_results
            .saturating_add(usize::from(verification));
        self.unknown_invalidations = self
            .unknown_invalidations
            .saturating_add(usize::from(unknown));
        self.completion_proposals = self
            .completion_proposals
            .saturating_add(usize::from(completion));
        if known_mutation || verification {
            self.outcome_advances = self.outcome_advances.saturating_add(1);
        } else {
            self.no_outcome_results = self.no_outcome_results.saturating_add(1);
        }
    }
}

/// Raw assistant evidence is ephemeral runtime state, not task authority.
/// Carry both identities so a later `/done` can never attach the previous
/// task's response after a focus switch, and diagnostics can name the exact
/// turn that produced the artifact.
#[derive(Debug, Clone)]
struct AssistantArtifactEvidence {
    task_id: TaskId,
    turn_id: TurnId,
    reference: String,
}

/// one deterministically-refused edit attempt. The
/// `OperationAttemptKey` is tool + argument digest; the trusted target
/// observations (`path@digest`) captured at refusal time prove the retry
/// would hit identical file identities.
#[derive(Debug, Clone)]
struct EditAttemptFact {
    tool_name: String,
    argument_digest: String,
    targets: Vec<String>,
    failure_class: agent_contracts::ToolFailureClass,
}

/// 一次程序解析失败的可证等价记录。参数摘要覆盖 argv0/cwd/env
/// 覆盖项；世界版本未推进 ⇒ 解析输入未变 ⇒ 同参数重试可证等价，可
/// 硬拒绝。残余假设：运行外环境（如 PATH）变化不在观察模型内，由
/// 收敛债务软性兜底；不做按名字的 K-strikes 硬封禁——listing 有界、
/// PATH/扩展名/后续构建都可能改变结论。
#[derive(Debug, Clone)]
struct LaunchResolutionFact {
    tool_name: String,
    argument_digest: String,
    argv0: String,
    workspace_revision: u64,
    failure_class: agent_contracts::ToolFailureClass,
}

/// Bounded attempt ledger: the retry loop the runtime cares about is
/// recent, not historical.
const MAX_EDIT_ATTEMPTS: usize = 8;

impl ActiveTurn {
    /// Whether `call` repeats a deterministic edit refusal whose target
    /// identities are all still Fresh and unchanged. Such a call is
    /// provably going to fail the same way, so the runtime refuses it
    /// without dispatch. Process/shell tools are never deduplicated:
    /// time and environment make them non-deterministic.
    fn duplicate_edit_attempt(&self, call: &ToolCall) -> Option<EditAttemptFact> {
        if !matches!(call.name.as_str(), "edit.replace" | "edit.patch") {
            return None;
        }
        let digest = ArgumentDigest::from_json(&call.arguments).to_string();
        self.edit_attempts
            .iter()
            .find(|attempt| {
                attempt.tool_name == call.name
                    && attempt.argument_digest == digest
                    && attempt.targets.iter().all(|target| {
                        target.split_once('@').is_some_and(|(path, digest)| {
                            self.execution.fact_for(path).is_some_and(|fact| {
                                fact.freshness == ResourceFreshness::Fresh && fact.digest == digest
                            })
                        })
                    })
            })
            .cloned()
    }

    /// Record (or clear) the attempt for one completed edit call.
    fn record_edit_attempt(&mut self, output: &ToolOutput, digest: &ArgumentDigest) {
        if !matches!(output.tool_name.as_str(), "edit.replace" | "edit.patch") {
            return;
        }
        let digest = digest.to_string();
        // A success consumed the attempt: the world moved enough for the
        // same arguments to apply, so a later retry is not a duplicate.
        if output.ok {
            self.edit_attempts.retain(|attempt| {
                !(attempt.tool_name == output.tool_name && attempt.argument_digest == digest)
            });
            return;
        }
        let Some(class) = output.failure_class() else {
            return;
        };
        if !matches!(
            class,
            agent_contracts::ToolFailureClass::StaleRevision
                | agent_contracts::ToolFailureClass::NoExactMatch
                | agent_contracts::ToolFailureClass::AmbiguousMatch
        ) {
            return;
        }
        // the refusal's trusted path+revision stamps are the
        // file identities a retry must still match to be a duplicate.
        let targets: Vec<String> = output
            .resource_touches()
            .into_iter()
            .filter_map(|touch| {
                touch
                    .revision
                    .filter(|revision| !revision.is_empty())
                    .map(|revision| format!("{}@{}", touch.path, revision))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        self.edit_attempts.retain(|attempt| {
            !(attempt.tool_name == output.tool_name && attempt.argument_digest == digest)
        });
        self.edit_attempts.push(EditAttemptFact {
            tool_name: output.tool_name.clone(),
            argument_digest: digest,
            targets,
            failure_class: class,
        });
        let excess = self.edit_attempts.len().saturating_sub(MAX_EDIT_ATTEMPTS);
        self.edit_attempts.drain(0..excess);
    }

    /// Whether `call` repeats a provably-equivalent launch failure: same
    /// tool, same argument digest (covers argv0/cwd/env overrides), and
    /// no world-revision advance since the failure. Such a retry resolves
    /// the same program and fails the same way; the runtime refuses it
    /// without dispatch.
    fn duplicate_launch_failure(&self, call: &ToolCall) -> Option<LaunchResolutionFact> {
        if !matches!(call.name.as_str(), "process.run" | "shell.exec") {
            return None;
        }
        let digest = ArgumentDigest::from_json(&call.arguments).to_string();
        self.launch_failures
            .iter()
            .find(|fact| {
                fact.tool_name == call.name
                    && fact.argument_digest == digest
                    && fact.workspace_revision == self.execution.workspace_revision
            })
            .cloned()
    }

    /// Record (or clear) the launch-failure domain for one completed
    /// process/shell call. A success consumes the attempt: the world or
    /// environment moved enough for the same arguments to resolve now.
    fn record_launch_failure(&mut self, output: &ToolOutput, digest: &ArgumentDigest) {
        if !matches!(output.tool_name.as_str(), "process.run" | "shell.exec") {
            return;
        }
        let digest = digest.to_string();
        if output.ok {
            self.launch_failures.retain(|fact| {
                !(fact.tool_name == output.tool_name && fact.argument_digest == digest)
            });
            return;
        }
        // 只有可证等价域（程序解析）入账；超时/退出码等非确定失败
        // 永不硬拒绝，只走收敛债务。
        if output.failure_domain() != agent_contracts::ToolFailureDomain::ExecutableResolution {
            return;
        }
        let Some(argv0) = output
            .metadata
            .get("argv0")
            .and_then(|value| value.as_str())
            .filter(|argv0| !argv0.is_empty())
            .map(str::to_owned)
        else {
            return;
        };
        let Some(class) = output.failure_class() else {
            return;
        };
        self.launch_failures
            .retain(|fact| !(fact.tool_name == output.tool_name && fact.argument_digest == digest));
        self.launch_failures.push(LaunchResolutionFact {
            tool_name: output.tool_name.clone(),
            argument_digest: digest,
            argv0,
            workspace_revision: self.execution.workspace_revision,
            failure_class: class,
        });
        let excess = self.launch_failures.len().saturating_sub(MAX_EDIT_ATTEMPTS);
        self.launch_failures.drain(0..excess);
    }

    /// 把成功观察到的正文记入当轮缓存。唯一正文来源
    /// 是 fs.read——edit 的 model_content 是 patch echo 不是完整文件，
    /// 不得冒充 exact body。失效规则：任何 Known mutation 使被触 path
    /// 失效；Unknown mutation 全部作废。
    fn record_protocol_body(&mut self, output: &ToolOutput) {
        if !output.ok {
            return;
        }
        let touches = output.resource_touches();
        match output.mutation_footprint() {
            agent_contracts::MutationFootprint::Unknown => {
                // Unknown 只挂起不删除——字节保留、资格
                // 冻结；BeforeModel 重验证同身份后可再次回注。
                self.protocol_bodies.suspend_all();
            }
            agent_contracts::MutationFootprint::Known(mutated) => {
                for touch in mutated {
                    self.protocol_bodies.invalidate_path(&touch.path);
                }
            }
            agent_contracts::MutationFootprint::None => {}
        }
        if output.tool_name != "fs.read" {
            return;
        }
        let Some(touch) = touches.first() else {
            return;
        };
        let Some(digest) = touch.revision.clone().filter(|digest| !digest.is_empty()) else {
            return;
        };
        self.protocol_bodies
            .record(&touch.path, &digest, &output.model_content);
    }
}

#[cfg(test)]
mod edit_attempt_tests {
    use super::*;
    use agent_contracts::ToolFailureClass;

    fn turn_with_fact(path: &str, digest: &str, freshness: ResourceFreshness) -> ActiveTurn {
        let mut turn = ActiveTurn {
            turn_id: TurnId::new(),
            turn_frame: TurnFrame::new("edit the file"),
            model_round: 1,
            pending_tools: VecDeque::new(),
            pending_loaded_tools: Vec::new(),
            result_delivery_tools: Vec::new(),
            action_batch: None,
            tool_surface: None,
            turn_state: TurnState::Running,
            op: None,
            execution: ExecutionState::default(),
            edit_attempts: Vec::new(),
            launch_failures: Vec::new(),
            protocol_bodies: crate::execution::body_cache::ProtocolBodyCache::default(),
            round_snapshot: None,
            pending_completion: None,
            opportunity_lease: None,
            recovery_surface_request: None,
            applied_input: None,
            input_consumed: false,
            structurally_empty_retries: 0,
            pending_scope_closes: VecDeque::new(),
        };
        turn.execution
            .checked_files
            .push(crate::execution::ResourceFact {
                path: path.into(),
                digest: digest.into(),
                freshness,
                turn: 1,
                provenance: crate::execution::ResourceProvenance::Read,
            });
        turn
    }

    fn refused_output(path: &str, revision: &str, class: ToolFailureClass) -> ToolOutput {
        ToolOutput {
            call_id: "c".into(),
            tool_name: "edit.replace".into(),
            ok: false,
            summary: "refused".into(),
            model_content: "refused".into(),
            artifact_ref: None,
            metadata: serde_json::json!({
                "path": path,
                "revision": revision,
                "failure_class": class.as_str(),
            }),
        }
    }

    fn edit_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "edit.replace".into(),
            arguments,
        }
    }

    #[test]
    fn identical_retry_against_unchanged_facts_is_a_duplicate() {
        let args = serde_json::json!({"path": "src/a.rs", "old": "x", "new": "y"});
        let call = edit_call(args.clone());
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        assert!(
            turn.duplicate_edit_attempt(&call).is_none(),
            "no recorded attempt yet"
        );

        turn.record_edit_attempt(
            &refused_output("src/a.rs", "rev1", ToolFailureClass::NoExactMatch),
            &ArgumentDigest::from_json(&args),
        );
        assert!(
            turn.duplicate_edit_attempt(&call).is_some(),
            "identical arguments against an unchanged Fresh fact is a duplicate"
        );

        // Different arguments: the model changed strategy — dispatch.
        let mut changed_args = args.clone();
        changed_args["new"] = serde_json::json!("z");
        assert!(
            turn.duplicate_edit_attempt(&edit_call(changed_args))
                .is_none()
        );

        // The file moved on: the retry would see different content.
        turn.execution.checked_files[0].digest = "rev2".into();
        assert!(turn.duplicate_edit_attempt(&call).is_none());

        // The fact identity is unproven (NeedsRevalidation): fail open.
        turn.execution.checked_files[0].digest = "rev1".into();
        turn.execution.checked_files[0].freshness = ResourceFreshness::NeedsRevalidation;
        assert!(turn.duplicate_edit_attempt(&call).is_none());
    }

    #[test]
    fn a_later_success_consumes_the_attempt() {
        let args = serde_json::json!({"path": "src/a.rs", "old": "x", "new": "y"});
        let digest = ArgumentDigest::from_json(&args);
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        turn.record_edit_attempt(
            &refused_output("src/a.rs", "rev1", ToolFailureClass::StaleRevision),
            &digest,
        );
        assert!(
            turn.duplicate_edit_attempt(&edit_call(args.clone()))
                .is_some()
        );
        // The same arguments now succeed (the world moved enough): the
        // attempt is consumed so a later retry dispatches again.
        let mut success = refused_output("src/a.rs", "rev1", ToolFailureClass::StaleRevision);
        success.ok = true;
        turn.record_edit_attempt(&success, &digest);
        assert!(turn.duplicate_edit_attempt(&edit_call(args)).is_none());
    }

    #[test]
    fn non_dedupable_failures_and_tools_stay_out_of_the_ledger() {
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        let args = serde_json::json!({"path": "src/a.rs", "old": "x", "new": "y"});
        let digest = ArgumentDigest::from_json(&args);
        // A process failure never enters the ledger.
        let mut process_failure =
            refused_output("src/a.rs", "rev1", ToolFailureClass::NoExactMatch);
        process_failure.tool_name = "shell.exec".into();
        process_failure.metadata = serde_json::json!({
            "command": "cargo test",
            "failure_class": "process_exit",
        });
        turn.record_edit_attempt(&process_failure, &digest);
        assert!(turn.edit_attempts.is_empty());
        // An edit failure without a stamped revision cannot be proven
        // deterministic: no ledger entry.
        let mut no_revision = refused_output("src/a.rs", "", ToolFailureClass::NoExactMatch);
        no_revision
            .metadata
            .as_object_mut()
            .unwrap()
            .remove("revision");
        turn.record_edit_attempt(&no_revision, &digest);
        assert!(turn.edit_attempts.is_empty());
    }

    // ---- 程序解析重试域（可证等价才硬拒绝）----

    fn launch_failure(argv0: &str, class: ToolFailureClass) -> ToolOutput {
        ToolOutput {
            call_id: "c".into(),
            tool_name: "process.run".into(),
            ok: false,
            summary: format!("program `{argv0}` was not found"),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({
                "argv": [argv0],
                "cwd": ".",
                "argv0": argv0,
                "failure_class": class.as_str(),
            }),
        }
    }

    fn launch_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c2".into(),
            name: "process.run".into(),
            arguments,
        }
    }

    #[test]
    fn identical_launch_retry_at_unchanged_revision_is_provably_equivalent() {
        let args = serde_json::json!({"argv": ["protocol_tests.exe"], "cwd": "."});
        let call = launch_call(args.clone());
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        assert!(turn.duplicate_launch_failure(&call).is_none());

        turn.record_launch_failure(
            &launch_failure("protocol_tests.exe", ToolFailureClass::PathNotFound),
            &ArgumentDigest::from_json(&args),
        );
        assert!(
            turn.duplicate_launch_failure(&call).is_some(),
            "same argv/cwd + unchanged world revision is a provable retry"
        );

        // 世界推进（例如刚完成一次构建）：结论可能改变，必须放行。
        turn.execution.workspace_revision += 1;
        assert!(turn.duplicate_launch_failure(&call).is_none());
    }

    #[test]
    fn launch_success_consumes_the_resolution_fact() {
        let args = serde_json::json!({"argv": ["app.exe"], "cwd": "."});
        let digest = ArgumentDigest::from_json(&args);
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        turn.record_launch_failure(
            &launch_failure("app.exe", ToolFailureClass::PathNotFound),
            &digest,
        );
        assert!(
            turn.duplicate_launch_failure(&launch_call(args.clone()))
                .is_some()
        );

        let mut success = launch_failure("app.exe", ToolFailureClass::PathNotFound);
        success.ok = true;
        success.metadata["exit_code"] = serde_json::json!(0);
        turn.record_launch_failure(&success, &digest);
        assert!(turn.duplicate_launch_failure(&launch_call(args)).is_none());
    }

    #[test]
    fn non_deterministic_and_non_process_failures_stay_out_of_the_launch_ledger() {
        let mut turn = turn_with_fact("src/a.rs", "rev1", ResourceFreshness::Fresh);
        // 退出码失败属于非确定域：重试结果不可证，永不硬拒绝。
        turn.record_launch_failure(
            &launch_failure("cargo", ToolFailureClass::ProcessExit),
            &ArgumentDigest::from_json(&serde_json::json!({"argv": ["cargo"]})),
        );
        // 文件工具的 PathNotFound 是资源路径域，不是程序解析。
        let mut fs_miss = launch_failure("missing.txt", ToolFailureClass::PathNotFound);
        fs_miss.tool_name = "fs.read".into();
        turn.record_launch_failure(
            &fs_miss,
            &ArgumentDigest::from_json(&serde_json::json!({"path": "missing.txt"})),
        );
        assert!(turn.launch_failures.is_empty());
    }
}

/// The commit lifecycle of a turn. `ModelFinished` means the model has
/// answered but the runtime has not persisted this turn yet; only after
/// every mandatory state write succeeds does the turn reach `Committed`
/// and `TurnCompleted` is emitted. A turn that fails mid-commit is dropped
/// and `RecoveryRequired` is journaled — "the model answered" and "the
/// runtime durably committed this turn" are two different facts, and the
/// latter is the only one that completes a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    /// The turn is executing (model round or tool loop in flight).
    Running,
    /// The model produced its final message; finalization has not started.
    ModelFinished,
    /// Finalization is writing the mandatory state (ingest/maintain/GC).
    Committing,
    /// Every mandatory state write succeeded; the turn is durable.
    Committed,
}

/// Which mandatory finalization step failed, so the journaled
/// `TurnCommitFailed` names the exact place recovery must look at.
#[derive(Debug, Clone, Copy)]
enum TurnCommitPhase {
    ToolObservationIngest,
    AfterToolMaintain,
    AfterToolMaintainedEvent,
    AssistantMessageArtifact,
    AssistantMessageIngest,
    AssistantMessageEvent,
    AfterModelMaintain,
    AfterModelMaintainedEvent,
    Gc,
    GcEvent,
    TurnCompletedEvent,
}

impl TurnCommitPhase {
    fn as_str(self) -> &'static str {
        match self {
            TurnCommitPhase::ToolObservationIngest => "tool_observation_ingest",
            TurnCommitPhase::AfterToolMaintain => "after_tool_maintain",
            TurnCommitPhase::AfterToolMaintainedEvent => "after_tool_maintained_event",
            TurnCommitPhase::AssistantMessageArtifact => "assistant_message_artifact",
            TurnCommitPhase::AssistantMessageIngest => "assistant_message_ingest",
            TurnCommitPhase::AssistantMessageEvent => "assistant_message_event",
            TurnCommitPhase::AfterModelMaintain => "after_model_maintain",
            TurnCommitPhase::AfterModelMaintainedEvent => "after_model_maintained_event",
            TurnCommitPhase::Gc => "gc",
            TurnCommitPhase::GcEvent => "gc_event",
            TurnCommitPhase::TurnCompletedEvent => "turn_completed_event",
        }
    }
}

/// What a spawned operation reports when it finishes.
pub(crate) struct OperationCompletion {
    operation: OperationResult,
    kind: OpKind,
    /// A staged side effect the tool prepared but did not apply. The actor
    /// commits it only after the generation fence passes; a stale operation
    /// must roll it back so the effect never lands (tool computation is
    /// separate from side-effect commit).
    effect: Option<Box<dyn Effect>>,
    /// The short-lived authority lease minted for this operation's side
    /// effect (ACI v2 §6). The actor validates it again at commit time —
    /// operation generation must match and the lease must not have expired
    /// — before any staged effect lands; a refused lease rolls the effect
    /// back and reports a failed tool result.
    lease: Option<AuthorityLease>,
    /// Core-assigned identity of a prepared world mutation, if any.
    effect_id: Option<EffectId>,
    /// Digest Core admitted for this tool operation.
    argument_digest: Option<ArgumentDigest>,
    /// Host purpose plus Runtime task relevance captured before dispatch.
    /// Output metadata cannot modify this after the operation runs.
    attribution: Option<RuntimeExecutionAttribution>,
    /// Original call retained only for exact verification. Runtime recomputes
    /// host identity after execution and records a reusable PASS only when the
    /// pre/post identities match; external changes during a verifier cannot
    /// manufacture a receipt for a mixed world.
    verification_call: Option<ToolCall>,
    /// Exact Core operation identity for tool cancellation/late-result
    /// terminalization. Model operations do not enter this registry.
    tool_identity: Option<ToolOperationIdentity>,
    /// Core keeps a dispatched non-effect operation in `Executing` until
    /// Runtime admits its result after the current-turn fence. Refusals and
    /// prepared effects are terminalized through their own paths.
    value_completion_pending: bool,
    /// Core detected unresolved preparation cleanup while executing the tool.
    /// This is trusted, bounded, and always fences later mutation.
    recovery_required: Option<String>,
    /// A runtime directive (context control) the tool attached to its
    /// output. Executed at commit time, right after the effect — so a
    /// "manual collect now" is actually now, not at turn end.
    directive: Option<RuntimeDirective>,
    /// Whether the result persists as a long-term observation at turn end.
    /// Engine-query results (context search/inspect/fetch) are transient:
    /// they must not duplicate fetched evidence under a new id.
    disposition: ToolResultDisposition,
    /// Exact final context projection associated with a model operation.
    /// Committed only after a non-stale successful ModelOutput; tool,
    /// failed and cancelled operations carry/commit none.
    context_ack: Option<ContextConsumptionAck>,
}

/// Actor-side restore data that cannot be published until the host has
/// applied the independent capability plane. Keeping the bounded event
/// fields here makes the interval explicit: while this value exists the
/// actor stays behind `recovery_required` and accepts no normal mutation.
struct PendingRestore {
    restore_id: u64,
    checkpoint_version: u32,
    restored_run_id: RunId,
    focus_revision: RestoreRevision,
    surface_revision: RestoreRevision,
    rebased_tasks: usize,
    rebased_task_sample: Vec<TaskId>,
}

/// Mutable runtime state, owned exclusively by the actor loop. Callers never
/// touch it: everything goes through `RuntimeCommand`.
#[derive(Default)]
struct ActorState {
    /// Focus epoch. Bumped on every accepted turn, focus change and cancel;
    /// operations tagged with an older generation are stale.
    generation: u64,
    /// Runtime-owned revision of the active focus input used to explain the
    /// source of a derived round surface. This is independent of the
    /// operation generation fence above.
    focus_revision: u64,
    /// Last issued immutable round-surface identity. Persisted in the
    /// runtime checkpoint so restore never reuses a revision.
    last_surface_revision: u64,
    /// The task the runtime believes is current (updated by `set_focus`).
    task_id: Option<TaskId>,
    /// Long-lived task records; focus is attention inside the current task.
    tasks: TaskManager,
    /// Per-process CAS high-water marks survive live restore even when an
    /// older checkpoint temporarily removes a task. Old actor handles cannot
    /// exist after a process restart, so this fence is intentionally not
    /// checkpoint authority.
    task_requirement_high_water: HashMap<TaskId, u64>,
    /// The runtime can no longer prove that ordinary mutation has a safe
    /// base: for example, a context rollback failed, a mandatory audit gap
    /// opened, or an effect was applied without a durable/knowable outcome.
    /// No later command mutation is accepted until the process is recovered
    /// from a known-good RuntimeCheckpoint.
    recovery_required: bool,
    /// A full restore whose actor-owned planes are installed but whose host
    /// capability plane and durable commit record are not yet finalized.
    pending_restore: Option<PendingRestore>,
    /// The runtime's view of the current scope (filled once the context
    /// engine exposes its scope tree through the contract).
    scope_id: Option<ScopeId>,
    /// The tool call currently executing, if any. The active-call policy
    /// reads this at the BeforeModel safe point so the round that consumes
    /// a tool's result still offers the tool.
    active_tool: Option<String>,
    turn: Option<ActiveTurn>,
    /// 上次随 ExecutionFrontier 上报的派生结算标签。进程内记忆，
    /// 不进 checkpoint：标签是重算派生态，checkpoint 不携带决策。
    last_reported_settlement: Option<agent_contracts::SettlementLabel>,
    /// A cancelled tool operation whose completion may still own a prepared
    /// effect. Ordinary mutation waits for this single cleanup result; Stop
    /// drains it with a hard deadline so dropping the actor never silently
    /// replaces explicit rollback.
    pending_tool_cleanup: Option<OperationId>,
    /// Ref of the most recent assistant-response artifact (raw-evidence
    /// retention): `finalize_turn` records it after persisting the full
    /// final response, and `commit_completion` attaches it to the task's
    /// CompletionRecord so the complete raw output is reachable even when
    /// the model's self-declared artifact list omits it.
    last_assistant_artifact: Option<AssistantArtifactEvidence>,
    /// Per-user-turn discovery search caps. Reset in `start_turn`.
    discovery_budget: DiscoveryTurnBudget,
    /// 周转中最多一条待处理对话（CTX-EVENT-02）。进程内有效，不进 checkpoint。
    pending_user_input: Option<QueuedUserDialogue>,
    /// Coalesced reasons the next settled batch owes a durable resume
    /// checkpoint. Read-only exploration leaves this empty.
    checkpoint_debt: Vec<crate::checkpoint::CheckpointDebtReason>,
    /// In-flight background checkpoint write plus the snapshot identity it
    /// was captured under and the exact debt set it froze, so a durable ack
    /// retires only what the artifact actually covers and newer debt
    /// survives for the following capture.
    checkpoint_write: Option<self::safepoint::InFlightCheckpoint>,
    /// Last background write ended in `CheckpointWriteFailed` and has not
    /// been followed by a durable one yet.
    checkpoint_write_failed: bool,
    /// Actor-owned monotonic allocator for safe-point snapshots. Independent
    /// of task-anchor revisions so same-anchor distinct snapshots and
    /// cross-task switches stay ordered; persisted through the checkpoint
    /// lineage so a restore cannot move it backwards.
    snapshot_sequence: u64,
    /// Durability watermarks for one process segment as snapshot sequences:
    /// the latest frozen snapshot continuation waits on, and the latest
    /// durably acknowledged sequence. Process-local by design — a restored
    /// runtime inherits durability through the loaded checkpoint itself.
    required_sequence: Option<u64>,
    durable_sequence: Option<u64>,
    /// Turn id whose completion-gate refusal was already surfaced, so one
    /// unresolved proposal warns once instead of every round.
    completion_refusal_surfaced_for: Option<TurnId>,
}

/// 已入账但尚未开转的对话。`input` 是 Queued 信封，`content` 是 ingest 全文。
struct QueuedUserDialogue {
    content: String,
    input: RuntimeInputEnvelope,
}

pub(crate) struct RuntimeActor {
    /// Narrow authority port: events, approval, effect admission/commit,
    /// output, and tool-execution wiring. Component authority objects and
    /// the concrete Core implementation never enter Runtime. Scheduling —
    /// context maintenance, model calls, tool lifecycle — lives on `services`.
    core: Arc<dyn CorePort>,
    /// The scheduling seam: context/model/tool/config operations the actor
    /// triggers. The actor decides every trigger and order; the services
    /// execute the call.
    services: Arc<RuntimeServices>,
    /// Owns the system prompt and Runtime Facts and renders the model input.
    /// The context engine only ever returns structured items.
    assembler: PromptAssembler,
    state: ActorState,
}

impl RuntimeActor {
    pub(crate) fn new(core: Arc<dyn CorePort>, services: Arc<RuntimeServices>) -> Self {
        let recovery_required = matches!(
            core.recovery_status(),
            agent_contracts::AuthorityRecoveryStatus::RecoveryRequired { .. }
        );
        let state = ActorState {
            generation: core.current_authority_epoch(),
            recovery_required,
            ..ActorState::default()
        };
        Self {
            assembler: {
                let assembler = PromptAssembler::new(services.system_prompt());
                match services.artifact_workspace() {
                    Some(workspace) => assembler.with_runtime_facts(workspace.runtime_facts()),
                    None => assembler,
                }
            },
            core,
            services,
            state,
        }
    }

    fn refresh_runtime_fact_markers(&mut self) {
        let Some(workspace) = self.services.artifact_workspace() else {
            return;
        };
        self.assembler.refresh_markers(workspace.project_markers());
    }

    /// 任务感知结算 join:`SettledCandidate` 仅当 execution-local 就绪
    /// (最大 `VerifiedCurrent`) 且 active task anchor 同意——epoch 匹配、
    /// 无 open loop、无 next action、每条 acceptance criterion 都有解析
    /// 到当前 trusted pass 的覆盖 claim——且无 in-flight 操作 / cancel
    /// cleanup。否则保留 execution 局部标签。in-flight 指尚未终止的
    /// 派发操作与待派发队列 (`active_tool` 是表面 pin,不在此列)。
    fn task_settlement_label(
        &self,
        execution: &crate::execution::ExecutionState,
    ) -> agent_contracts::SettlementLabel {
        use agent_contracts::SettlementLabel;
        let local = execution.settlement();
        if local != SettlementLabel::VerifiedCurrent {
            return local;
        }
        let in_flight_clear = self.state.pending_tool_cleanup.is_none()
            && self.state.turn.as_ref().is_none_or(|turn| {
                turn.op.is_none() && turn.pending_tools.is_empty()
            });
        let ready = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
            .is_some_and(|task| {
                crate::task::task_ready(&task.anchor, execution, in_flight_clear)
                    && task.status != crate::task::TaskStatus::Completed
            });
        if ready {
            SettlementLabel::SettledCandidate
        } else {
            SettlementLabel::VerifiedCurrent
        }
    }

    /// Run the actor loop until `Stop` or all handles are dropped. Commands
    /// and operation completions are handled concurrently so cancellation
    /// can always be processed while an operation is running.
    pub(crate) async fn run(
        mut self,
        mut rx: mpsc::Receiver<RuntimeCommand>,
        op_tx: mpsc::Sender<OperationCompletion>,
        mut op_rx: mpsc::Receiver<OperationCompletion>,
    ) {
        loop {
            tokio::select! {
                command = rx.recv() => {
                    match command {
                        Some(RuntimeCommand::Stop { reply }) => {
                            let result = self.shutdown(&mut op_rx, &op_tx).await;
                            let _ = reply.send(result);
                            return;
                        }
                        Some(command) => self.process(command, &op_tx).await,
                        // Every caller handle was dropped: the run must still
                        // shut down cleanly (cancel, flush, RunCompleted)
                        // instead of silently returning.
                        None => {
                            let _ = self.shutdown(&mut op_rx, &op_tx).await;
                            return;
                        }
                    }
                }
                completed = op_rx.recv() => {
                    match completed {
                        Some(completion) => self.on_operation_completed(completion, &op_tx).await,
                        None => {
                            let _ = self.shutdown(&mut op_rx, &op_tx).await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Ordered teardown for the actor: cancel any in-flight turn, then stop
    /// the kernel (journal flush + `RunCompleted`). The kernel stop result is
    /// returned so a flush failure reaches the caller.
    async fn shutdown(
        &mut self,
        op_rx: &mut mpsc::Receiver<OperationCompletion>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) -> AgentResult<()> {
        let cancel = self
            .cancel_turn(TurnCancellationReason::Shutdown, None)
            .await;
        self.state.pending_user_input = None;
        let cleanup = self.drain_cancelled_tool_cleanup(op_rx, op_tx).await;
        let stop = self.core.stop().await;
        let mut errors = Vec::new();
        if let Err(error) = cancel {
            errors.push(format!("turn cancellation failed: {error}"));
        }
        if let Err(error) = cleanup {
            errors.push(format!("cancelled operation cleanup failed: {error}"));
        }
        if let Err(error) = stop {
            errors.push(format!("runtime stop failed: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(AgentError::RecoveryRequired(errors.join("; ")))
        }
    }

    async fn drain_cancelled_tool_cleanup(
        &mut self,
        op_rx: &mut mpsc::Receiver<OperationCompletion>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) -> AgentResult<()> {
        let Some(target) = self.state.pending_tool_cleanup else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + SHUTDOWN_OPERATION_DRAIN_TIMEOUT;
        while self.state.pending_tool_cleanup == Some(target) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, op_rx.recv()).await {
                Ok(Some(completion)) => self.on_operation_completed(completion, op_tx).await,
                Ok(None) | Err(_) => break,
            }
        }
        if self.state.pending_tool_cleanup == Some(target) {
            self.state.recovery_required = true;
            let message = format!(
                "cancelled tool operation {target} did not return for explicit effect cleanup before the shutdown deadline"
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: message.clone(),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            Err(AgentError::RecoveryRequired(message))
        } else {
            Ok(())
        }
    }
}

/// Start the runtime: obtain the narrow authority port from the services,
/// create the actor task and hand back a cloneable handle. The port is shared
/// with the handle (same event channel and run identity).
pub fn spawn_runtime(services: Arc<RuntimeServices>) -> (RuntimeHandle, JoinHandle<()>) {
    let core = services.core_port();
    let (tx, rx) = mpsc::channel(64);
    let (op_tx, op_rx) = mpsc::channel(16);
    let actor = RuntimeActor::new(core.clone(), services);
    let handle = RuntimeHandle::new(tx, core.event_sender(), core.run_id());
    let task = tokio::spawn(actor.run(rx, op_tx, op_rx));
    (handle, task)
}

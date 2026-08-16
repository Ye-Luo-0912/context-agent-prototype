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
    AuthorityRecoveryStatus, CAPABILITY_MANAGE, CONTEXT_CONSUMPTION_ACK_ITEM_CAP,
    CancellationToken, CompletionProposal, ContextConsumptionAck, ContextHints, ContextIngress,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextRetention,
    DISCOVERY_IDENTICAL_QUERY_BUDGET, DISCOVERY_MAX_QUERIES_PER_TURN, DiscoveryBudgetExhausted,
    DiscoveryTurnBudget, Effect, EffectDurability, EffectId, EffectReceipt, InputAuthority,
    InputKind, InputLifecycle, InputSource, MAX_COMPLETION_ARTIFACTS, ModelInput, ModelRequest,
    OperationId, OperationOutcome, OperationQueryResult, OperationResult, OperationState,
    OperationTerminal, RestoreRevision, RunId, RuntimeDirective, RuntimeEvent,
    RuntimeInputEnvelope, RuntimeInputId, ScopeId, ScopeKind, StatePatchProposal, TaskId, ToolCall,
    ToolOperationIdentity, ToolOutcome, ToolOutput, ToolResultDisposition, ToolSurfaceBlock,
    ToolSurfaceBlockReason, ToolSurfaceDemand, ToolSurfaceSnapshot, TurnCancelAck,
    TurnCancellationReason, TurnFrame, TurnFrameStep, TurnId, USER_INPUT_ARTIFACT_OWNER,
    USER_INPUT_PREVIEW_CHARS, USER_INPUT_QUEUE_CAP, bounded_preview, discovery_search_from_call,
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
use crate::checkpoint::{
    RUNTIME_CHECKPOINT_VERSION, RunMetadata, RuntimeCheckpoint, TaskManagerSnapshot,
};
use crate::command::{Reply, RuntimeCommand, RuntimeHandle};
use crate::output::bound_tool_output;
use crate::prompt::PromptAssembler;
use crate::services::RuntimeServices;
use crate::sink::LiveSink;
use crate::surface::{RoundSurfacePlan, SurfaceReportContext};
use crate::task::{
    AnchorPatch, TaskManager, changed_fields_kind, normalize_tool_requirements,
    validate_completion_proposal,
};

mod lifecycle;
mod model;
mod restore;
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
    /// The tool surface of the current model round, captured once after the
    /// tool lifecycle GC. `None` only before the first round starts.
    tool_surface: Option<ToolSurfaceSnapshot>,
    /// Where the turn is in its commit lifecycle (see `TurnState`).
    turn_state: TurnState,
    op: Option<InFlightOp>,
    /// A structured completion proposal the model attached to a tool call
    /// (`task.complete`). Committed at the turn's safe point — after the
    /// turn commits — through the CTX-10 transaction, so completion never
    /// races an in-flight operation.
    pending_completion: Option<CompletionProposal>,
    /// 本轮 Applied 对话。Consumed / Archived / InterruptCommitted 复用同一条。
    applied_input: Option<RuntimeInputEnvelope>,
    /// 已经为这条 applied input 发过 Consumed。
    input_consumed: bool,
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
    /// Exact Core operation identity for tool cancellation/late-result
    /// terminalization. Model operations do not enter this registry.
    tool_identity: Option<ToolOperationIdentity>,
    /// Core keeps a dispatched non-effect operation in `Executing` until
    /// Runtime admits its result after the current-turn fence. Refusals and
    /// prepared effects are terminalized through their own paths.
    value_completion_pending: bool,
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
    /// CTX-DISC-03: per-user-turn discovery search caps. Reset in `start_turn`.
    discovery_budget: DiscoveryTurnBudget,
    /// 周转中最多一条待处理对话（CTX-EVENT-02）。进程内有效，不进 checkpoint。
    pending_user_input: Option<QueuedUserDialogue>,
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
    /// Owns the system prompt and renders the five-layer model input. The
    /// context engine only ever returns structured items.
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
            assembler: PromptAssembler::new(services.system_prompt()),
            core,
            services,
            state,
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

    async fn process(
        &mut self,
        command: RuntimeCommand,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        match command {
            RuntimeCommand::Start { reply } => {
                let _ = reply.send(self.core.start().await);
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
                        // can never diverge.
                        let (txn, task_id) = self.state.tasks.prepare_create(&goal);
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
                let result =
                    match self.ensure_idle() {
                        Err(error) => Err(error),
                        Ok(()) => match self.state.tasks.prepare_patch_anchor(
                            task_id,
                            base_revision,
                            &patch,
                        ) {
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
                                                        Ok(revision)
                                                    }
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
            RuntimeCommand::Pin { content, reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => {
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
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::CompleteTask { summary, reply } => {
                let result = match self.ensure_idle().and_then(|_| self.next_focus_revision()) {
                    Ok(next_focus_revision) => {
                        self.commit_completion(summary, Vec::new(), next_focus_revision)
                            .await
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Checkpoint { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => match self.core.checkpoint().await {
                        Ok(context) => self.core.authority_checkpoint_marker().map(|authority| {
                            RuntimeCheckpoint {
                                version: RUNTIME_CHECKPOINT_VERSION,
                                run_metadata: RunMetadata {
                                    run_id: self.core.run_id(),
                                    created_at_ms: now_ms(),
                                },
                                tasks: TaskManagerSnapshot::from_manager(&self.state.tasks),
                                current_task_id: self.state.task_id,
                                focus_revision: self.state.focus_revision,
                                last_surface_revision: self.state.last_surface_revision,
                                context,
                                // The actor does not own the host: the capability
                                // surface is merged in by RuntimeInstance.
                                capabilities: Vec::new(),
                                authority,
                            }
                        }),
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::PrepareRestore { checkpoint, reply } => {
                let result = self.prepare_restore(checkpoint).await;
                let _ = reply.send(result);
            }
            RuntimeCommand::FinalizeRestore {
                restore_id,
                capabilities_applied,
                reply,
            } => {
                let result = self
                    .finalize_restore(restore_id, capabilities_applied)
                    .await;
                let _ = reply.send(result);
            }
            RuntimeCommand::EmitDiagnostics { reply } => {
                // Pure read of engine state: allowed at any time.
                let _ = reply.send(self.core.emit_diagnostics().await);
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
                let result = self.cancel_operation(identity).await;
                if result.is_ok() {
                    self.drain_queued_user_input(op_tx).await;
                }
                let _ = reply.send(result);
            }
            RuntimeCommand::CancelTurn { reply } => {
                let result = self
                    .cancel_turn(TurnCancellationReason::Requested, None)
                    .await;
                if result.is_ok() {
                    self.drain_queued_user_input(op_tx).await;
                }
                let _ = reply.send(result);
            }
            RuntimeCommand::Stop { .. } => unreachable!("Stop is handled in the run loop"),
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

#[cfg(test)]
mod restore_tests {
    use super::*;
    use agent_contracts::{
        ArgumentDigest, AuthorityRecoveryStatus, ContextDiagnostics, ContextEngine, ContextIngress,
        ContextItemSummary, ContextMaintenanceReport, EffectId, MaterializedContext,
        ModelCapabilities, ModelOutput, ModelTransport, OPERATION_JOURNAL_VERSION,
        OperationJournal, OperationJournalRecord, OperationJournalRecovery,
        OperationJournalTransition, OperationQueryResult, OperationSnapshot, OperationState,
        OperationTerminal, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOperationIdentity,
        ToolRisk, ToolSpec,
    };
    use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
    use async_trait::async_trait;
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    use tokio::sync::{mpsc, oneshot};

    #[derive(Debug)]
    struct TestContext;

    #[async_trait]
    impl ContextEngine for TestContext {
        async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
            Ok(())
        }
        async fn maintain(
            &self,
            _trigger: ContextMaintenanceTrigger,
        ) -> AgentResult<ContextMaintenanceReport> {
            Ok(ContextMaintenanceReport::default())
        }
        async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
            Ok(MaterializedContext {
                materialization_id: 0,
                focus: None,
                task: None,
                items: Vec::new(),
                external: Default::default(),
                selected: Vec::new(),
                approx_tokens: 0,
                diagnostics: ContextDiagnostics::default(),
            })
        }
        async fn open_scope(
            &self,
            _kind: ScopeKind,
            _parent: Option<ScopeId>,
        ) -> AgentResult<ScopeId> {
            Ok(ScopeId::new())
        }
        async fn close_scope(
            &self,
            _scope_id: ScopeId,
        ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
            Ok(Vec::new())
        }
        async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
            Ok(ContextDiagnostics::default())
        }
        async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
            Ok(Vec::new())
        }
        async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }
        async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct TestModel;

    #[async_trait]
    impl ModelTransport for TestModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            unreachable!("restore fencing test never reaches the model")
        }
    }

    #[derive(Debug)]
    struct TestTools;

    #[async_trait]
    impl ToolDispatcher for TestTools {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }
        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            unreachable!("restore fencing test never executes a tool")
        }
    }

    struct RecoveredOperationJournal {
        recovery: OperationJournalRecovery,
        seq: AtomicU64,
        transitions: Mutex<Vec<OperationJournalTransition>>,
    }

    impl OperationJournal for RecoveredOperationJournal {
        fn append_and_sync(
            &self,
            transition: &OperationJournalTransition,
        ) -> AgentResult<OperationJournalRecord> {
            let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
            self.transitions.lock().unwrap().push(transition.clone());
            Ok(OperationJournalRecord {
                version: OPERATION_JOURNAL_VERSION,
                seq,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<OperationJournalRecovery> {
            Ok(self.recovery.clone())
        }
    }

    struct FailAtOperationJournal {
        recovery: OperationJournalRecovery,
        seq: AtomicU64,
        fail_at: AtomicU64,
        transitions: Mutex<Vec<OperationJournalTransition>>,
    }

    impl OperationJournal for FailAtOperationJournal {
        fn append_and_sync(
            &self,
            transition: &OperationJournalTransition,
        ) -> AgentResult<OperationJournalRecord> {
            let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
            if self.fail_at.load(Ordering::Acquire) == seq {
                return Err(AgentError::Storage(
                    "injected operation authority journal failure".into(),
                ));
            }
            self.transitions.lock().unwrap().push(transition.clone());
            Ok(OperationJournalRecord {
                version: OPERATION_JOURNAL_VERSION,
                seq,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<OperationJournalRecovery> {
            Ok(self.recovery.clone())
        }
    }

    #[derive(Debug, Default)]
    struct OneToolModel {
        rounds: AtomicUsize,
    }

    #[async_trait]
    impl ModelTransport for OneToolModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                tool_calls: true,
                ..ModelCapabilities::default()
            }
        }

        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            if self.rounds.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "late-value-call".into(),
                        name: "test.late_value".into(),
                        arguments: serde_json::json!({}),
                    }],
                    usage: Default::default(),
                })
            } else {
                Ok(ModelOutput {
                    content: "done".into(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                })
            }
        }
    }

    /// Deliberately violates cooperative cancellation: its result arrives
    /// only after the test releases it, even if Runtime cancelled its token.
    #[derive(Debug)]
    struct LateValueTool {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        request_identity: Option<Arc<Mutex<Option<ToolOperationIdentity>>>>,
        dispatch_completed: Option<Arc<tokio::sync::Notify>>,
        risk: ToolRisk,
    }

    #[async_trait]
    impl ToolDispatcher for LateValueTool {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "test.late_value".into(),
                description: "returns a late read-only value".into(),
                input_schema: serde_json::json!({"type": "object"}),
                risk: self.risk,
                output_budget: None,
            }]
        }

        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            if let Some(identity) = &self.request_identity {
                *identity.lock().unwrap() = request
                    .effect_context
                    .as_ref()
                    .map(|context| context.identity.clone());
            }
            self.entered.notify_one();
            self.release.notified().await;
            if let Some(completed) = &self.dispatch_completed {
                completed.notify_one();
            }
            Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "late value".into(),
                model_content: "late value".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            }))
        }
    }

    fn checkpoint(run_id: RunId) -> RuntimeCheckpoint {
        RuntimeCheckpoint {
            version: RUNTIME_CHECKPOINT_VERSION,
            run_metadata: RunMetadata {
                run_id,
                created_at_ms: 0,
            },
            tasks: TaskManagerSnapshot {
                tasks: Vec::new(),
                active: None,
                completed: Vec::new(),
            },
            current_task_id: None,
            focus_revision: 0,
            last_surface_revision: 0,
            context: serde_json::Value::Null,
            capabilities: Vec::new(),
            authority: None,
        }
    }

    async fn process_command(actor: &mut RuntimeActor, command: RuntimeCommand) {
        let (op_tx, _op_rx) = mpsc::channel(1);
        actor.process(command, &op_tx).await;
    }

    #[test]
    fn actor_starts_fenced_when_core_recovery_is_unresolved() {
        let effect_id = EffectId::new();
        let operation_id = OperationId::new();
        let journal = Arc::new(RecoveredOperationJournal {
            recovery: OperationJournalRecovery {
                authority_epoch: 4,
                operations: vec![OperationSnapshot {
                    identity: ToolOperationIdentity {
                        run_id: RunId::new(),
                        task_id: None,
                        turn_id: TurnId::new(),
                        scope_id: None,
                        operation_id,
                        generation: 4,
                        call_id: "generic-process".into(),
                        tool_name: "process.run".into(),
                        argument_digest: ArgumentDigest::sha256_bytes(b"process args"),
                    },
                    state: OperationState::Executing {
                        effect_id: Some(effect_id),
                    },
                }],
                ..OperationJournalRecovery::default()
            },
            seq: AtomicU64::new(0),
            transitions: Mutex::new(Vec::new()),
        });
        let services = Arc::new(
            RuntimeServices::try_new(
                CoreAuthorityConfig::default(),
                Arc::new(TestContext),
                Arc::new(TestModel),
                Arc::new(TestTools),
                Arc::new(PolicyApprovalGate::read_only()),
                None,
                crate::services::AuthorityRecoveryServices::new(journal, None),
            )
            .unwrap(),
        );
        let core = services.core_port();
        assert!(matches!(
            core.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));

        let actor = RuntimeActor::new(core, services);
        assert!(actor.state.recovery_required);
        assert_eq!(actor.state.generation, 5);
    }

    #[tokio::test]
    async fn operation_query_remains_available_behind_the_recovery_fence() {
        let effect_id = EffectId::new();
        let operation_id = OperationId::new();
        let identity = ToolOperationIdentity {
            run_id: RunId::new(),
            task_id: None,
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id,
            generation: 4,
            call_id: "query-recovery".into(),
            tool_name: "process.run".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"query recovery"),
        };
        let journal = Arc::new(RecoveredOperationJournal {
            recovery: OperationJournalRecovery {
                authority_epoch: 4,
                operations: vec![OperationSnapshot {
                    identity: identity.clone(),
                    state: OperationState::Executing {
                        effect_id: Some(effect_id),
                    },
                }],
                ..OperationJournalRecovery::default()
            },
            seq: AtomicU64::new(0),
            transitions: Mutex::new(Vec::new()),
        });
        let services = Arc::new(
            RuntimeServices::try_new(
                CoreAuthorityConfig::default(),
                Arc::new(TestContext),
                Arc::new(TestModel),
                Arc::new(TestTools),
                Arc::new(PolicyApprovalGate::read_only()),
                None,
                crate::services::AuthorityRecoveryServices::new(journal, None),
            )
            .unwrap(),
        );
        let mut actor = RuntimeActor::new(services.core_port(), services);
        assert!(actor.state.recovery_required);

        let (query_tx, query_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::QueryOperation {
                operation_id,
                reply: query_tx,
            },
        )
        .await;
        let OperationQueryResult::Found { snapshot } = query_rx.await.unwrap().unwrap() else {
            panic!("recovery tooling must retain exact operation truth")
        };
        assert_eq!(snapshot.identity, identity);

        let (cancel_tx, cancel_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::CancelOperation {
                identity,
                reply: cancel_tx,
            },
        )
        .await;
        assert!(matches!(
            cancel_rx.await.unwrap(),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[tokio::test]
    async fn prepared_restore_stays_fenced_and_unpublished_until_finalize() {
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(TestModel),
            Arc::new(TestTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ));
        let core = services.core_port();
        let mut events = core.event_sender().subscribe();
        let run_id = core.run_id();
        let mut actor = RuntimeActor::new(core, services);

        let (prepare_tx, prepare_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::PrepareRestore {
                checkpoint: checkpoint(run_id),
                reply: prepare_tx,
            },
        )
        .await;
        let restore_id = prepare_rx.await.unwrap().unwrap();

        assert!(actor.state.recovery_required);
        assert!(actor.state.pending_restore.is_some());
        assert!(events.try_recv().is_err(), "prepare must publish no event");

        let (mutation_tx, mutation_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::SetFocus {
                goal: "must remain fenced".into(),
                reply: mutation_tx,
            },
        )
        .await;
        assert!(matches!(
            mutation_rx.await.unwrap(),
            Err(AgentError::RecoveryRequired(_))
        ));

        // A newer prepare replaces the pending attempt. Its token prevents
        // a delayed finalize from the old caller from committing/unfencing
        // this newer actor state.
        let (new_prepare_tx, new_prepare_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::PrepareRestore {
                checkpoint: checkpoint(run_id),
                reply: new_prepare_tx,
            },
        )
        .await;
        let new_restore_id = new_prepare_rx.await.unwrap().unwrap();
        assert_ne!(restore_id, new_restore_id);

        let (stale_tx, stale_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::FinalizeRestore {
                restore_id,
                capabilities_applied: true,
                reply: stale_tx,
            },
        )
        .await;
        assert!(matches!(
            stale_rx.await.unwrap(),
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(actor.state.recovery_required);
        assert_eq!(
            actor
                .state
                .pending_restore
                .as_ref()
                .map(|pending| pending.restore_id),
            Some(new_restore_id)
        );
        assert!(
            events.try_recv().is_err(),
            "stale finalize publishes nothing"
        );

        let (finalize_tx, finalize_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::FinalizeRestore {
                restore_id: new_restore_id,
                capabilities_applied: true,
                reply: finalize_tx,
            },
        )
        .await;
        finalize_rx.await.unwrap().unwrap();
        assert!(!actor.state.recovery_required);
        assert!(actor.state.pending_restore.is_none());
        let restored = events.try_recv().expect("finalize publishes restore event");
        assert!(matches!(
            restored.event,
            RuntimeEvent::RuntimeRestored {
                capabilities_applied: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn actor_epoch_mismatch_poison_is_fail_closed() {
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(TestModel),
            Arc::new(TestTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ));
        let core = services.core_port();
        let mut actor = RuntimeActor::new(core.clone(), services);
        let actor_epoch = actor.state.generation;
        core.advance_authority_epoch(actor_epoch).unwrap();

        assert!(matches!(
            actor.bump_generation(),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert!(actor.state.recovery_required);
        assert_eq!(actor.state.generation, actor_epoch);

        let (mutation_tx, mutation_rx) = oneshot::channel();
        process_command(
            &mut actor,
            RuntimeCommand::SetFocus {
                goal: "must remain fenced".into(),
                reply: mutation_tx,
            },
        )
        .await;
        assert!(matches!(
            mutation_rx.await.unwrap(),
            Err(AgentError::RecoveryRequired(_))
        ));
    }

    #[tokio::test]
    async fn cancelled_late_tool_value_stays_cancelled_in_core_operation_truth() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(OneToolModel::default()),
            Arc::new(LateValueTool {
                entered: entered.clone(),
                release: release.clone(),
                request_identity: None,
                dispatch_completed: None,
                risk: ToolRisk::ReadOnly,
            }),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ));
        let core = services.core_port();
        let (handle, task) = spawn_runtime(services);
        let mut events = handle.subscribe();
        handle.start().await.unwrap();
        handle
            .user_message("run the late tool".into())
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("tool did not enter execution");
        let cancelled_operation = match handle.cancel_turn().await.unwrap() {
            TurnCancelAck::Cancelled {
                operation_id: Some(operation_id),
                ..
            } => operation_id,
            acknowledgement => {
                panic!("active tool cancellation must name its operation: {acknowledgement:?}")
            }
        };
        let OperationQueryResult::Found { snapshot } = core.query_operation(cancelled_operation)
        else {
            panic!("the cancelled operation must remain queryable after turn cancellation")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::CancelledBeforeCommit,
            }
        ));

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let envelope = events.recv().await.unwrap();
                if matches!(
                    envelope.event,
                    RuntimeEvent::Warning { ref message }
                        if message.contains("stale tool result dropped")
                ) {
                    break;
                }
            }
        })
        .await
        .expect("actor did not process the late tool completion");

        let OperationQueryResult::Found { snapshot } = core.query_operation(cancelled_operation)
        else {
            panic!("the terminal operation must remain queryable")
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::CancelledBeforeCommit,
            }
        ));

        handle.stop().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn precise_operation_cancel_rejects_identity_drift_and_returns_core_truth() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let observed_identity = Arc::new(Mutex::new(None));
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(OneToolModel::default()),
            Arc::new(LateValueTool {
                entered: entered.clone(),
                release: release.clone(),
                request_identity: Some(observed_identity.clone()),
                dispatch_completed: None,
                risk: ToolRisk::WorkspaceWrite,
            }),
            Arc::new(PolicyApprovalGate::permissive()),
            None,
        ));
        let (handle, task) = spawn_runtime(services);
        handle.start().await.unwrap();
        handle
            .user_message("run the precise-cancel tool".into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("tool did not enter execution");
        let identity = observed_identity
            .lock()
            .unwrap()
            .clone()
            .expect("Core must pass the admitted operation identity to the dispatcher");

        let OperationQueryResult::Found { snapshot } =
            handle.query_operation(identity.operation_id).await.unwrap()
        else {
            panic!("the active operation must be queryable")
        };
        assert_eq!(snapshot.identity, identity);
        assert!(matches!(
            snapshot.state,
            OperationState::Executing { effect_id: Some(_) }
        ));

        let mut drifted = identity.clone();
        drifted.call_id.push_str("-forged");
        assert!(matches!(
            handle.cancel_operation(drifted).await,
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(matches!(
            handle.query_operation(identity.operation_id).await.unwrap(),
            OperationQueryResult::Found { ref snapshot }
                if matches!(snapshot.state, OperationState::Executing { .. })
        ));

        let OperationQueryResult::Found { snapshot } =
            handle.cancel_operation(identity.clone()).await.unwrap()
        else {
            panic!("successful precise cancellation must return retained Core truth")
        };
        assert_eq!(snapshot.identity, identity);
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                effect_id: Some(_),
                terminal: OperationTerminal::CancelledBeforeCommit,
            }
        ));

        assert!(matches!(
            handle.cancel_operation(identity).await,
            Err(AgentError::InvalidRequest(_))
        ));
        release.notify_one();
        handle.stop().await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn operation_cancel_losing_to_a_core_terminal_returns_truth_without_fencing() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let dispatch_completed = Arc::new(tokio::sync::Notify::new());
        let observed_identity = Arc::new(Mutex::new(None));
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContext),
            Arc::new(OneToolModel::default()),
            Arc::new(LateValueTool {
                entered: entered.clone(),
                release: release.clone(),
                request_identity: Some(observed_identity.clone()),
                dispatch_completed: Some(dispatch_completed.clone()),
                risk: ToolRisk::WorkspaceWrite,
            }),
            Arc::new(PolicyApprovalGate::permissive()),
            None,
        ));
        let core = services.core_port();
        let mut actor = RuntimeActor::new(core.clone(), services);
        let (op_tx, mut op_rx) = mpsc::channel(1);

        let (focus_tx, focus_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::SetFocus {
                    goal: "late cancel race".into(),
                    reply: focus_tx,
                },
                &op_tx,
            )
            .await;
        focus_rx.await.unwrap().unwrap();
        let (message_tx, message_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::UserMessage {
                    content: "run the late tool".into(),
                    reply: message_tx,
                },
                &op_tx,
            )
            .await;
        message_rx.await.unwrap().unwrap();
        actor
            .on_operation_completed(op_rx.recv().await.unwrap(), &op_tx)
            .await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("tool did not enter execution");
        let identity = observed_identity
            .lock()
            .unwrap()
            .clone()
            .expect("side-effecting dispatch carries the admitted identity");

        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), dispatch_completed.notified())
            .await
            .expect("tool dispatch did not finish");
        let completion = op_rx.recv().await.expect("tool completion must be queued");
        core.cancel_operation(identity.clone()).unwrap();
        let epoch_before = actor.state.generation;

        let (cancel_tx, cancel_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::CancelOperation {
                    identity: identity.clone(),
                    reply: cancel_tx,
                },
                &op_tx,
            )
            .await;
        let OperationQueryResult::Found { snapshot } = cancel_rx.await.unwrap().unwrap() else {
            panic!("a cancellation race loser must receive retained Core truth")
        };
        assert_eq!(snapshot.identity, identity);
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                terminal: OperationTerminal::CancelledBeforeCommit,
                ..
            }
        ));
        assert_eq!(actor.state.generation, epoch_before);
        assert!(!actor.state.recovery_required);
        assert!(actor.state.turn.is_some());

        actor.on_operation_completed(completion, &op_tx).await;
    }

    #[tokio::test]
    async fn partial_atomic_cancel_wal_failure_fences_actor_and_stays_queryable() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let observed_identity = Arc::new(Mutex::new(None));
        let journal = Arc::new(FailAtOperationJournal {
            recovery: OperationJournalRecovery::default(),
            seq: AtomicU64::new(0),
            fail_at: AtomicU64::new(u64::MAX),
            transitions: Mutex::new(Vec::new()),
        });
        let services = Arc::new(
            RuntimeServices::try_new(
                CoreAuthorityConfig::default(),
                Arc::new(TestContext),
                Arc::new(OneToolModel::default()),
                Arc::new(LateValueTool {
                    entered: entered.clone(),
                    release: release.clone(),
                    request_identity: Some(observed_identity.clone()),
                    dispatch_completed: None,
                    risk: ToolRisk::WorkspaceWrite,
                }),
                Arc::new(PolicyApprovalGate::permissive()),
                None,
                crate::services::AuthorityRecoveryServices::new(journal.clone(), None),
            )
            .unwrap(),
        );
        let core = services.core_port();
        let mut actor = RuntimeActor::new(core.clone(), services);
        let (op_tx, mut op_rx) = mpsc::channel(1);

        let (focus_tx, focus_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::SetFocus {
                    goal: "atomic cancellation WAL fault".into(),
                    reply: focus_tx,
                },
                &op_tx,
            )
            .await;
        focus_rx.await.unwrap().unwrap();
        let (message_tx, message_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::UserMessage {
                    content: "run the tool".into(),
                    reply: message_tx,
                },
                &op_tx,
            )
            .await;
        message_rx.await.unwrap().unwrap();
        actor
            .on_operation_completed(op_rx.recv().await.unwrap(), &op_tx)
            .await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("tool did not enter execution");
        let identity = observed_identity
            .lock()
            .unwrap()
            .clone()
            .expect("side-effecting dispatch carries the admitted identity");

        // The atomic cancel writes EpochAdvanced first and the exact
        // cancellation terminal second. Fail only the second record: Core
        // must keep its in-memory epoch/state unpublished and both layers
        // must become observably fenced.
        let next = journal.seq.load(Ordering::Acquire) + 2;
        journal.fail_at.store(next, Ordering::Release);
        let mut events = core.event_sender().subscribe();
        let generation_before = actor.state.generation;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        actor
            .process(
                RuntimeCommand::CancelOperation {
                    identity: identity.clone(),
                    reply: cancel_tx,
                },
                &op_tx,
            )
            .await;
        assert!(matches!(
            cancel_rx.await.unwrap(),
            Err(AgentError::RecoveryRequired(_))
        ));
        assert!(actor.state.recovery_required);
        assert_eq!(actor.state.generation, generation_before);
        assert_eq!(core.current_authority_epoch(), generation_before);
        assert!(matches!(
            core.recovery_status(),
            AuthorityRecoveryStatus::RecoveryRequired { .. }
        ));
        assert!(matches!(
            core.query_operation(identity.operation_id),
            OperationQueryResult::Found { ref snapshot }
                if matches!(snapshot.state, OperationState::Executing { .. })
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap()
                .event,
            RuntimeEvent::RecoveryRequired
        ));

        release.notify_one();
        let completion = tokio::time::timeout(Duration::from_secs(2), op_rx.recv())
            .await
            .expect("late tool completion timed out")
            .expect("late tool completion channel closed");
        actor.on_operation_completed(completion, &op_tx).await;
    }
}

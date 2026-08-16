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

    /// Install the actor-owned planes of a runtime checkpoint, but do not
    /// claim that the full runtime is restored yet. This mutation is
    /// deliberately allowed while the actor-local event/context fence is
    /// raised, but it cannot clear an unresolved Core authority fence. On
    /// success the actor remains fenced until the host applies capabilities
    /// and calls `finalize_restore`.
    async fn prepare_restore(&mut self, checkpoint: RuntimeCheckpoint) -> AgentResult<u64> {
        self.ensure_no_active_turn()?;
        checkpoint.validate()?;
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
        } = checkpoint;

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
        });
        Ok(restore_id)
    }

    /// Prove that a checkpoint belongs to the live Core authority lineage
    /// before any epoch, context, task, or capability-plane mutation. A
    /// marker is an ancestor cross-check only: its state is never installed
    /// into Core. Ephemeral checkpoints cannot prove cross-process lineage,
    /// so they are restricted to the same live run.
    fn validate_restore_authority(&self, checkpoint: &RuntimeCheckpoint) -> AgentResult<()> {
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
    async fn finalize_restore(
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

    /// A turn is accepted only when the runtime is idle. Serializing every
    /// mutation removes the structural race where focus/pin/task commands
    /// interleaved with an in-flight turn.
    fn ensure_idle(&self) -> AgentResult<()> {
        if self.state.recovery_required {
            Err(AgentError::RecoveryRequired(
                "runtime recovery is required before normal mutation may continue".into(),
            ))
        } else if let Some(operation_id) = self.state.pending_tool_cleanup {
            Err(AgentError::InvalidRequest(format!(
                "agent is finishing explicit cleanup for cancelled tool operation {operation_id}"
            )))
        } else {
            self.ensure_no_active_turn()
        }
    }

    /// Ask the approval gate whether a boundary anchor patch (goal /
    /// constraints / waiver) may proceed. The patch is presented as a
    /// synthetic `task.anchor` tool call so existing approval policies (and
    /// the v2 shadow gate) see a typed, serializable request instead of a
    /// side channel. The gate decides; a deny or a failed check errors out
    /// without touching the task table.
    async fn authorize_anchor_patch(&self, patch: &AnchorPatch) -> AgentResult<()> {
        let arguments = serde_json::to_value(patch).map_err(|error| {
            AgentError::Internal(format!("anchor patch serialization: {error}"))
        })?;
        let call = ToolCall {
            id: format!("anchor-patch-{}", RunId::new()),
            name: "task.anchor".into(),
            arguments,
        };
        let spec = agent_contracts::ToolSpec {
            name: "task.anchor".into(),
            description: "Patch the task anchor; goal/constraint fields require approval".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
            output_budget: None,
        };
        let verdict = self
            .core
            .authorize(&call, &spec, &CancellationToken::new())
            .await;
        match verdict {
            ApprovalVerdict::Allowed => Ok(()),
            ApprovalVerdict::Denied(message) | ApprovalVerdict::Failed(message) => {
                Err(AgentError::InvalidRequest(format!(
                    "boundary anchor patch denied by approval policy: {message}"
                )))
            }
        }
    }

    fn next_focus_revision(&self) -> AgentResult<u64> {
        self.state
            .focus_revision
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("runtime focus revision is exhausted".into()))
    }

    /// Ask trusted Core to advance the process-lifetime commit fence. The
    /// actor remains the sole lifecycle scheduler; Core owns only the
    /// monotonic authority value and rejects stale or forged commits.
    fn bump_generation(&mut self) -> AgentResult<u64> {
        match self.core.advance_authority_epoch(self.state.generation) {
            Ok(epoch) => {
                self.state.generation = epoch;
                Ok(epoch)
            }
            Err(error) => {
                self.state.recovery_required = true;
                Err(error)
            }
        }
    }

    fn issue_surface_revision(&mut self) -> AgentResult<u64> {
        let revision = self
            .state
            .last_surface_revision
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("round surface revision is exhausted".into()))?;
        self.state.last_surface_revision = revision;
        Ok(revision)
    }

    fn ensure_no_active_turn(&self) -> AgentResult<()> {
        if self.state.turn.is_some() {
            Err(AgentError::InvalidRequest(
                "agent is busy: a turn is already running".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn context_transition_failed(&mut self, error: AgentError) -> AgentError {
        if matches!(&error, AgentError::RecoveryRequired(_)) {
            self.state.recovery_required = true;
            let core = self.core.clone();
            tokio::spawn(async move {
                let _ = core.emit_event(RuntimeEvent::RecoveryRequired).await;
            });
        }
        error
    }

    /// Publish the audit/UI events for an already committed context/task
    /// transition. Event persistence may still fail, but it can no longer
    /// leave the context plane ahead of the task authority plane.
    async fn publish_context_transition(
        &mut self,
        event: RuntimeEvent,
        trigger: ContextMaintenanceTrigger,
        report: ContextMaintenanceReport,
    ) -> AgentResult<()> {
        if let Err(error) = self.core.emit_event(event).await {
            return Err(self.audit_gap_after_commit(error).await);
        }
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ContextMaintained { trigger, report })
            .await
        {
            return Err(self.audit_gap_after_commit(error).await);
        }
        Ok(())
    }

    async fn audit_gap_after_commit(&mut self, error: AgentError) -> AgentError {
        self.state.recovery_required = true;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        AgentError::RecoveryRequired(format!(
            "context/task transition committed, but its audit event failed ({error})"
        ))
    }

    /// GC/Storage GC 前把当前活跃任务的 anchor 根声明投影给引擎。
    /// ResidentRequired/PromptRequired 的声明保护（或召回）工作集条目，
    /// StorageRequired 的声明保护 store 留存。任务权威留在 TaskManager，
    /// 这里只导出有界投影；推送失败不阻塞 GC——引擎仍按已推送的根集
    /// 运行（失败以 Error 事件暴露，绝不静默）。`force` 时即使投影为空
    /// 也推送（完成边界用它清掉旧声明，让完成任务的记录不再被保护）；
    /// 否则空投影跳过，不打扰既有的 directive 语义。
    async fn push_anchor_roots_for_gc(&self, force: bool) {
        let roots = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| crate::task::anchor_root_claims(&task.anchor))
            .unwrap_or_default();
        if roots.is_empty() && !force {
            return;
        }
        if let Err(error) = self
            .services
            .context_ingest(ContextIngress::ContextDirective {
                action: agent_contracts::ContextAction::AnchorRoots { roots },
            })
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!("failed to push anchor roots before GC: {error}"),
                })
                .await;
        }
    }

    /// One full GC pass after a task completed, so the finished task's
    /// records leave the resident heap and stay recallable from the
    /// reversible buffer / context store. The completion itself is already
    /// committed; a GC failure is surfaced as an `Error` event and never
    /// rolls the outcome back.
    async fn compact_after_completion(&mut self) {
        // 完成边界前的根声明投影：完成任务后 active 通常已切换/清空，
        // 强制推送当前（或空）根集，声明不再保护已完成任务的工作集。
        self.push_anchor_roots_for_gc(true).await;
        match self.services.context_gc().await {
            Ok(report) => {
                if let Err(error) = self
                    .core
                    .emit_event(RuntimeEvent::ContextGc { report })
                    .await
                {
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!("post-completion GC failed: {error}"),
                    })
                    .await;
            }
        }
    }

    /// Task completion is an explicit runtime boundary for Storage GC: the
    /// completed task's records are storage roots until this point, after
    /// which the only live references are the completion outcome and its
    /// evidence. Run one conservative Storage GC pass here — never on the
    /// per-model hot path — and publish the report so every permanent
    /// deletion is observable and auditable. A failure is surfaced as an
    /// Error event, never allowed to undo the completed task.
    async fn run_storage_gc_at_boundary(&mut self) {
        // 完成边界前推送根声明投影：StorageRequired 的声明会让 storage GC
        // 保留其指向的 store 条目（已完成任务的证据留存由声明决定）。
        self.push_anchor_roots_for_gc(true).await;
        match self.services.context_storage_gc().await {
            Ok(report) => {
                if let Err(error) = self
                    .core
                    .emit_event(RuntimeEvent::StorageGc { report })
                    .await
                {
                    let _ = self
                        .core
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .core
                    .emit_event(RuntimeEvent::Error {
                        message: format!("storage GC at task completion failed: {error}"),
                    })
                    .await;
            }
        }
    }

    /// 把用户正文写入证据平面一次。没有 artifact workspace 时返回空引用，
    /// 事件仍然只带有界预览。
    async fn persist_user_input_body(
        &self,
        content: &str,
    ) -> AgentResult<(Option<String>, Option<String>)> {
        let Some(workspace) = self.services.artifact_workspace() else {
            return Ok((None, None));
        };
        let reference = workspace
            .write_artifact(
                self.core.run_id(),
                USER_INPUT_ARTIFACT_OWNER,
                "txt",
                content.as_bytes(),
            )
            .await?;
        let digest = ArtifactLocator::parse(&reference)?
            .digest()
            .map(|digest| digest.to_string());
        Ok((Some(reference), digest))
    }

    async fn emit_user_input(&self, input: RuntimeInputEnvelope) -> AgentResult<()> {
        if let Err(reason) = input.validate() {
            return Err(AgentError::InvalidRequest(reason));
        }
        self.core
            .emit_event(RuntimeEvent::UserMessageAccepted { input })
            .await
    }

    /// 清理中的 UserMessage fail closed，留下 Rejected。RecoveryRequired 是栅栏。
    async fn record_rejected_user_dialogue(&self, content: &str) -> AgentResult<()> {
        let input = RuntimeInputEnvelope::user_dialogue(
            content.to_owned(),
            Some(RuntimeInputId::new()),
            self.state.task_id,
            None,
            None,
            None,
        )
        .with_lifecycle(InputLifecycle::Rejected);
        self.emit_user_input(input).await
    }

    fn cancellation_preview(reason: TurnCancellationReason) -> &'static str {
        match reason {
            TurnCancellationReason::Requested => "cancel turn",
            TurnCancellationReason::OperationCancelled => "operation cancelled",
            TurnCancellationReason::Shutdown => "shutdown",
        }
    }

    async fn publish_interrupt_committed(
        &self,
        turn_id: TurnId,
        causal_parent: Option<RuntimeInputId>,
        reason: TurnCancellationReason,
    ) {
        let preview = Self::cancellation_preview(reason);
        let input = RuntimeInputEnvelope {
            preview: bounded_preview(preview, USER_INPUT_PREVIEW_CHARS),
            input_id: Some(RuntimeInputId::new()),
            task_id: self.state.task_id,
            turn_id: Some(turn_id),
            causal_parent,
            source: InputSource::User,
            authority: InputAuthority::UserSteering,
            kind: InputKind::CancelTurn,
            lifecycle: InputLifecycle::InterruptCommitted,
            body_ref: None,
            digest: None,
            bytes: preview.len() as u64,
            proposal: StatePatchProposal::None,
        };
        let _ = self.emit_user_input(input).await;
    }

    async fn emit_input_consumed(&mut self) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        if turn.input_consumed {
            return;
        }
        let Some(applied) = turn.applied_input.clone() else {
            return;
        };
        turn.input_consumed = true;
        let _ = self
            .emit_user_input(applied.with_lifecycle(InputLifecycle::Consumed))
            .await;
    }

    async fn emit_input_archived(&self, applied: RuntimeInputEnvelope) {
        let _ = self
            .emit_user_input(applied.with_lifecycle(InputLifecycle::Archived))
            .await;
    }

    /// 周转中最多排队 `USER_INPUT_QUEUE_CAP` 条。槽满则 Rejected。
    async fn queue_user_dialogue(&mut self, content: String) -> AgentResult<()> {
        if self.state.pending_user_input.is_some() {
            let _ = self.record_rejected_user_dialogue(&content).await;
            return Err(AgentError::InvalidRequest(format!(
                "agent is busy: a turn is already running and {USER_INPUT_QUEUE_CAP} user message is already queued"
            )));
        }
        let input_id = RuntimeInputId::new();
        let (body_ref, digest) = self.persist_user_input_body(&content).await?;
        let mut input = RuntimeInputEnvelope::user_dialogue(
            content.clone(),
            Some(input_id),
            self.state.task_id,
            None,
            body_ref,
            digest,
        )
        .with_lifecycle(InputLifecycle::Queued);
        input.causal_parent = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.applied_input.as_ref())
            .and_then(|applied| applied.input_id);
        self.emit_user_input(input.clone()).await?;
        self.state.pending_user_input = Some(QueuedUserDialogue { content, input });
        Ok(())
    }

    async fn drain_queued_user_input(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
        if self.state.recovery_required
            || self.state.turn.is_some()
            || self.state.pending_tool_cleanup.is_some()
        {
            return;
        }
        let Some(queued) = self.state.pending_user_input.take() else {
            return;
        };
        if let Err(error) = self
            .begin_applied_turn(queued.content, queued.input, op_tx)
            .await
        {
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: format!("queued user input failed to start: {error}"),
                })
                .await;
        }
    }

    /// Commit the turn start: user message into the long-term context, then
    /// spawn the first model operation.
    async fn start_turn(
        &mut self,
        content: String,
        reply: Reply<AgentResult<()>>,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        if self.state.recovery_required {
            let _ = reply.send(Err(AgentError::RecoveryRequired(
                "runtime recovery is required before normal mutation may continue".into(),
            )));
            return;
        }
        if let Some(operation_id) = self.state.pending_tool_cleanup {
            let error = AgentError::InvalidRequest(format!(
                "agent is finishing explicit cleanup for cancelled tool operation {operation_id}"
            ));
            let _ = self.record_rejected_user_dialogue(&content).await;
            let _ = reply.send(Err(error));
            return;
        }
        if content.trim().is_empty() {
            let _ = reply.send(Ok(()));
            return;
        }
        if self.state.turn.is_some() {
            let _ = reply.send(self.queue_user_dialogue(content).await);
            return;
        }

        let input_id = RuntimeInputId::new();
        let persist = match self.persist_user_input_body(&content).await {
            Ok(stored) => stored,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let input = RuntimeInputEnvelope::user_dialogue(
            content.clone(),
            Some(input_id),
            self.state.task_id,
            None,
            persist.0,
            persist.1,
        );
        let _ = reply.send(self.begin_applied_turn(content, input, op_tx).await);
    }

    async fn begin_applied_turn(
        &mut self,
        content: String,
        mut input: RuntimeInputEnvelope,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) -> AgentResult<()> {
        // Fence the new turn before an implicit focus or user-message write
        // becomes visible. If a later step fails, the unused epoch is safe;
        // an accepted turn can never run under an older Core authority.
        self.bump_generation()?;

        // The first message with no active task auto-creates one: a task is
        // the long-lived entity and the engine must never mint a TaskId, so
        // this is the single place an implicit task can be born. The focus
        // change lands before the message is ingested, exactly like an
        // explicit `/focus`.
        if self.state.tasks.active().is_none() {
            let next_focus_revision = self.next_focus_revision()?;
            let (txn, task_id) = self.state.tasks.prepare_create(content.trim());
            match self.services.set_focus(task_id, content.clone()).await {
                Err(error) => return Err(self.context_transition_failed(error)),
                Ok(report) => {
                    self.state.tasks.commit(txn);
                    self.state.task_id = Some(task_id);
                    self.state
                        .task_requirement_high_water
                        .entry(task_id)
                        .or_insert(0);
                    self.state.focus_revision = next_focus_revision;
                    self.publish_context_transition(
                        RuntimeEvent::FocusChanged {
                            task_id,
                            goal: content.clone(),
                        },
                        ContextMaintenanceTrigger::FocusChanged,
                        report,
                    )
                    .await?;
                }
            }
        }

        let turn_id = TurnId::new();
        if input.body_ref.is_none() {
            let (body_ref, digest) = self.persist_user_input_body(&content).await?;
            input.body_ref = body_ref;
            input.digest = digest;
        }
        input.turn_id = Some(turn_id);
        input.task_id = self.state.task_id;
        input.lifecycle = InputLifecycle::Applied;
        // ingest 成功后再发 Applied 事件，避免日志里有 Accepted 而上下文没有正文。
        self.services
            .context_ingest(ContextIngress::UserMessage {
                content: content.clone(),
            })
            .await?;
        let applied = input.with_lifecycle(InputLifecycle::Applied);
        self.emit_user_input(applied.clone()).await?;
        let report = self
            .services
            .context_maintain(ContextMaintenanceTrigger::UserInput)
            .await?;
        self.core
            .emit_event(RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::UserInput,
                report,
            })
            .await?;

        // A new turn has no active call from a previous turn: the
        // active-call policy only pins tools while the turn that issued
        // them still consumes their results.
        self.state.active_tool = None;
        self.state.discovery_budget.reset();
        self.state.turn = Some(ActiveTurn {
            turn_id,
            turn_frame: TurnFrame::new(content),
            model_round: 0,
            pending_tools: VecDeque::new(),
            tool_surface: None,
            turn_state: TurnState::Running,
            op: None,
            pending_completion: None,
            applied_input: Some(applied),
            input_consumed: false,
        });
        self.advance_turn(op_tx).await;
        Ok(())
    }

    /// Spawn the next operation the turn state says should run: a pending
    /// tool call, or the next model round. No-op while one is in flight.
    async fn advance_turn(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
        enum Action {
            Model,
            Tool(ToolCall),
        }
        let action = {
            let Some(turn) = self.state.turn.as_mut() else {
                return;
            };
            if turn.op.is_some() {
                return;
            }
            if let Some(call) = turn.pending_tools.pop_front() {
                Some(Action::Tool(call))
            } else {
                Some(Action::Model)
            }
        };
        let Some(action) = action else {
            return;
        };
        match action {
            Action::Model => self.spawn_next_model_or_end(op_tx).await,
            Action::Tool(call) => self.spawn_tool_operation(call, op_tx).await,
        }
    }

    async fn spawn_next_model_or_end(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
        let over_budget = self.state.turn.as_ref().is_some_and(|turn| {
            turn.op.is_none() && turn.model_round >= self.services.max_tool_rounds()
        });
        if over_budget {
            let message = format!(
                "tool round budget exhausted after {} rounds",
                self.services.max_tool_rounds()
            );
            let _ = self
                .core
                .emit_event(RuntimeEvent::Warning {
                    message: message.clone(),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::Error { message }).await;
            self.state.turn = None;
            return;
        }
        self.spawn_model_operation(op_tx).await;
    }

    /// Prepare + spawn one model round: close the consumed tool frames,
    /// maintenance, materialize, assemble, then the model call as an
    /// operation.
    async fn spawn_model_operation(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
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
                    .core
                    .emit_event(RuntimeEvent::ContextMaintained {
                        trigger: ContextMaintenanceTrigger::BeforeModel,
                        report,
                    })
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
        let model_budget = ModelBudget::compute(
            pack_window,
            output_reserve,
            self.assembler.system_prompt_tokens(),
            turn_frame_tokens,
            active_tools_tokens,
        );
        let materialize_started = std::time::Instant::now();
        // 当前活跃任务锚的根声明 + TaskAnchorView 投影：PromptRequired
        // 的声明会强制条目进帧；view 进 focus 帧，引擎不评分。任务权威
        // 留在 TaskManager，引擎只消费有界投影。
        let (anchor_roots, task_view) = self
            .state
            .tasks
            .active()
            .and_then(|task_id| self.state.tasks.get(task_id))
            .map(|task| {
                (
                    crate::task::anchor_root_claims(&task.anchor),
                    Some(crate::task::task_anchor_view(&task.anchor)),
                )
            })
            .unwrap_or((Vec::new(), None));
        let materialized = match self
            .services
            .context_materialize(ContextQuery {
                current_input: current_input.clone(),
                budget_tokens: model_budget.context_frame_budget,
                hints: ContextHints {
                    max_selected_items: Some(CONTEXT_CONSUMPTION_ACK_ITEM_CAP),
                    anchor_roots,
                    task: task_view,
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
            self.assembler
                .assemble(&materialized, &turn_frame, surface_plan.specs().to_vec());
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
            input =
                self.assembler
                    .assemble(&materialized, &turn_frame, surface_plan.specs().to_vec());
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
            input =
                self.assembler
                    .assemble(&materialized, &turn_frame, surface_plan.specs().to_vec());
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

    /// Prepare + spawn one tool call. Core first appends the exact operation
    /// identity to its authority WAL; only then does Runtime publish
    /// `OperationAccepted` / `ToolStarted` and consume the one-shot dispatch
    /// permit. This makes the event stream a safe discovery surface without
    /// turning it into operation authority.
    async fn spawn_tool_operation(
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
    async fn abort_admitted_tool_before_dispatch(
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
    async fn on_operation_completed(
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
    async fn require_effect_recovery(&mut self, warning: String) {
        let newly_required = !self.state.recovery_required;
        self.state.recovery_required = true;
        let _ = self.core.emit_warning(warning).await;
        if newly_required {
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        }
    }

    /// An operation is stale when the turn it belongs to is gone or the
    /// in-flight identity no longer matches (a cancel or a newer turn).
    fn is_stale(&self, completion: &OperationCompletion) -> bool {
        let Some(turn) = &self.state.turn else {
            return true;
        };
        turn.op.as_ref().is_none_or(|op| {
            op.operation_id != completion.operation.operation_id
                || op.turn_id != completion.operation.turn_id
                || op.generation != completion.operation.generation
        })
    }

    /// Execute a runtime directive a tool attached to its output. Runs at
    /// operation-commit time — after any staged effect, before the result
    /// enters the turn frame — so a "manual collect now" is actually now,
    /// and a hint/lease/tag lands before the next model round, not at turn
    /// end. Only trusted tools and `runtime:context-control` capabilities
    /// can produce directives (the dispatcher enforces that); here they are
    /// simply routed to the engine.
    async fn execute_directive(&mut self, directive: RuntimeDirective) {
        match directive {
            RuntimeDirective::Context(agent_contracts::ContextAction::Collect) => {
                // GC 前把当前活跃任务的 anchor 根声明投影给引擎：声明指向
                // 的条目在本次 pass 中受保护/召回，store 声明保护留存。
                // 推送失败不阻塞 collect——引擎仍按已推送的根集运行。
                // 空投影跳过（collect 本身不是 ingest directive）。
                self.push_anchor_roots_for_gc(false).await;
                match self.services.context_gc().await {
                    Ok(report) => {
                        if let Err(error) = self
                            .core
                            .emit_event(RuntimeEvent::ContextGc { report })
                            .await
                        {
                            // The GC state change landed but its audit
                            // event did not: surface it instead of letting
                            // the state silently outrun its journal event.
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::Error {
                                    message: error.to_string(),
                                })
                                .await;
                        }
                    }
                    Err(error) => {
                        // A failed explicit collect is not silent: the model
                        // asked for a pass and the engine refused it.
                        let _ = self
                            .core
                            .emit_event(RuntimeEvent::Error {
                                message: error.to_string(),
                            })
                            .await;
                    }
                }
            }
            RuntimeDirective::Context(other) => {
                if let Err(error) = self
                    .services
                    .context_ingest(ContextIngress::ContextDirective { action: other })
                    .await
                {
                    // A quota refused the directive (keep_alive / lease
                    // caps): the model believes it was granted, so surface
                    // the refusal.
                    let _ = self
                        .core
                        .emit_warning(format!("context directive refused: {error}"))
                        .await;
                }
            }
            RuntimeDirective::CompleteTask(proposal) => {
                // Validated and stored on the turn; the CTX-10 transaction
                // runs at the turn's safe point (after the turn commits),
                // never mid-operation, so the completion cannot race an
                // in-flight tool or model call.
                if let Err(error) = self.accept_completion_proposal(proposal) {
                    let _ = self
                        .core
                        .emit_warning(format!("completion proposal refused: {error}"))
                        .await;
                }
            }
        }
    }

    /// Validate and accept a structured completion proposal from
    /// `task.complete`. It is stored on the turn and committed at the
    /// turn's safe point; a later proposal replaces an earlier one. The
    /// model-facing tool result already told the model the proposal was
    /// submitted — the refusal path here only fires for malformed input.
    fn accept_completion_proposal(&mut self, proposal: CompletionProposal) -> AgentResult<()> {
        validate_completion_proposal(
            &proposal,
            self.services.artifact_workspace(),
            self.core.run_id(),
        )?;
        let Some(turn) = self.state.turn.as_mut() else {
            return Err(AgentError::InvalidRequest(
                "no active turn to complete".into(),
            ));
        };
        turn.pending_completion = Some(proposal);
        Ok(())
    }

    /// Commit the active task's typed CompletionRecord — the CTX-10
    /// transaction: prepare the record, run the engine's focus/context
    /// transition, commit the task flip, publish `TaskCompleted`, then one
    /// full GC pass so the completed task's records leave the resident
    /// heap (durable retention; a GC failure after the commit is surfaced,
    /// never allowed to undo the outcome). Shared by the `/done` command
    /// and the model's `task.complete` proposal.
    async fn commit_completion(
        &mut self,
        summary: String,
        artifacts: Vec<String>,
        next_focus_revision: u64,
    ) -> AgentResult<()> {
        let active_task = self
            .state
            .tasks
            .active()
            .ok_or_else(|| AgentError::InvalidRequest("no active task to complete".into()))?;

        // Revalidate at the commit safe point: acceptance and commit are
        // separated by the rest of the turn, so a referenced file may have
        // disappeared or changed type in between. Raw runtime evidence has
        // priority, then proposal refs retain their declared order. Canonical
        // locators make alias deduplication deterministic, and the persisted
        // record can never exceed the contract cap after adding raw evidence.
        let workspace = self.services.artifact_workspace().cloned();
        let raw_evidence = self
            .state
            .last_assistant_artifact
            .as_ref()
            .filter(|evidence| evidence.task_id == active_task)
            .cloned();
        let mut merged_artifacts = Vec::with_capacity(MAX_COMPLETION_ARTIFACTS);
        let mut seen = HashSet::with_capacity(MAX_COMPLETION_ARTIFACTS);
        if let Some(evidence) = raw_evidence {
            let validated = match workspace.as_ref() {
                Some(workspace) => workspace
                    .open_artifact_for_run(&evidence.reference, self.core.run_id())
                    .await
                    .map(|(normalized, _file)| normalized),
                None => Err(AgentError::InvalidRequest(
                    "raw assistant evidence requires a trusted artifact workspace".into(),
                )),
            };
            match validated {
                Ok(reference) => {
                    seen.insert(reference.clone());
                    merged_artifacts.push(reference);
                }
                Err(error) => {
                    // This is runtime-owned best-effort evidence: losing it
                    // must be visible but must not make `/done` permanently
                    // uncallable. Model-proposed refs below remain strict.
                    self.state.last_assistant_artifact = None;
                    let _ = self
                        .core
                        .emit_warning(format!(
                            "raw assistant evidence from turn {} was unavailable at completion: {error}",
                            evidence.turn_id
                        ))
                        .await;
                }
            }
        }

        let mut dropped_for_cap = 0usize;
        for artifact in artifacts {
            let Some(workspace) = workspace.as_ref() else {
                return Err(AgentError::InvalidRequest(
                    "completion artifacts require a trusted artifact workspace".into(),
                ));
            };
            let (normalized, _file) = workspace
                .open_artifact_for_run(&artifact, self.core.run_id())
                .await?;
            if !seen.insert(normalized.clone()) {
                continue;
            }
            if merged_artifacts.len() < MAX_COMPLETION_ARTIFACTS {
                merged_artifacts.push(normalized);
            } else {
                dropped_for_cap = dropped_for_cap.saturating_add(1);
            }
        }
        if dropped_for_cap > 0 {
            let _ = self
                .core
                .emit_warning(format!(
                    "completion artifact cap kept raw evidence first and omitted {dropped_for_cap} proposal ref(s)"
                ))
                .await;
        }
        // The exact final-output body is the completion summary itself in
        // this prototype: retain its digest so the outcome stays
        // byte-for-byte verifiable, with a deterministic ref naming the
        // task's completion record.
        let final_output_digest = Some(crate::task::sha256_hex(summary.as_bytes()));
        let final_output_ref = self
            .state
            .tasks
            .active()
            .map(|task_id| format!("task:{task_id}:completion"));
        let Some((txn, record)) = self.state.tasks.prepare_complete(
            summary.clone(),
            final_output_ref,
            final_output_digest,
            merged_artifacts,
        ) else {
            return Err(AgentError::InvalidRequest(
                "no active task to complete".into(),
            ));
        };
        let task_id = record.task_id;
        let anchor_revision = record.anchor_revision;
        let event_summary = record.summary.clone();
        self.bump_generation()?;
        let report = self
            .services
            .complete_current_task(task_id, summary)
            .await
            .map_err(|error| self.context_transition_failed(error))?;
        self.state.tasks.commit(txn);
        self.state.task_id = None;
        self.state.last_assistant_artifact = None;
        self.state.focus_revision = next_focus_revision;
        let transition = self
            .publish_context_transition(
                RuntimeEvent::TaskCompleted {
                    task_id,
                    anchor_revision,
                    summary: event_summary,
                },
                ContextMaintenanceTrigger::TaskCompleted,
                report,
            )
            .await;
        if transition.is_ok() {
            self.compact_after_completion().await;
            self.run_storage_gc_at_boundary().await;
        }
        transition
    }

    /// When the model stops calling tools, the turn's tool observations
    /// become the long-term record, then the final assistant message. Each
    /// observation is tagged with the tool scope that produced it. Context
    /// directives were already executed at operation-commit time (see
    /// `execute_directive`), so finalization only persists observations.
    ///
    /// Finalization is a commit: every mandatory state write (observation
    /// ingest, the maintenance passes, GC and their journal events) must
    /// succeed before the turn is `Committed` and `TurnCompleted` is
    /// emitted. On the first failure the commit aborts — later writes would
    /// build on a state that is already inconsistent — and the runtime
    /// journals `TurnCommitFailed` (naming the phase) plus
    /// `RecoveryRequired` instead of pretending the turn completed.
    async fn finalize_turn(&mut self, content: String) {
        let assistant_evidence_identity = self
            .state
            .task_id
            .zip(self.state.turn.as_ref().map(|turn| turn.turn_id));
        let mut pending_assistant_evidence = None;
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::ModelFinished;
        }
        let mut ingested = false;
        if let Some(turn) = self.state.turn.as_mut() {
            for step in &turn.turn_frame.steps {
                let TurnFrameStep::ToolResult {
                    output,
                    scope_id,
                    disposition,
                } = step
                else {
                    continue;
                };
                // Transient results (context search/inspect/fetch) stay out
                // of the long-term context: reading evidence must not
                // duplicate it under a new observation id. The engine
                // already stamped access on the read itself.
                if *disposition != ToolResultDisposition::PersistObservation {
                    continue;
                }
                if let Err(error) = self
                    .services
                    .context_ingest(ContextIngress::ToolObservation {
                        output: output.clone(),
                        scope_id: *scope_id,
                    })
                    .await
                {
                    return self
                        .commit_failed(TurnCommitPhase::ToolObservationIngest, error)
                        .await;
                }
                ingested = true;
            }
        }
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::Committing;
        }
        if ingested {
            let report = match self
                .services
                .context_maintain(ContextMaintenanceTrigger::AfterTool)
                .await
            {
                Ok(report) => report,
                Err(error) => {
                    return self
                        .commit_failed(TurnCommitPhase::AfterToolMaintain, error)
                        .await;
                }
            };
            if let Err(error) = self
                .core
                .emit_event(RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterTool,
                    report,
                })
                .await
            {
                return self
                    .commit_failed(TurnCommitPhase::AfterToolMaintainedEvent, error)
                    .await;
            }
        }
        // Raw-evidence retention: the exact final assistant response is
        // persisted in full *before* the bounded ContextItem is built, so
        // the raw output survives ContextItem truncation and stays
        // recoverable even when the engine's copy was capped. The artifact
        // name embeds a fresh uuid, so sibling responses never overwrite
        // each other. A failure here aborts the commit exactly like any
        // other mandatory state write.
        if let Some(workspace) = self.services.artifact_workspace() {
            use tokio::io::AsyncWriteExt;
            let mut draft = match workspace
                .create_artifact(self.core.run_id(), "assistant-response", "txt")
                .await
            {
                Ok(draft) => draft,
                Err(error) => {
                    return self
                        .commit_failed(TurnCommitPhase::AssistantMessageArtifact, error)
                        .await;
                }
            };
            if let Err(error) = draft.write_all(content.as_bytes()).await {
                return self
                    .commit_failed(
                        TurnCommitPhase::AssistantMessageArtifact,
                        AgentError::Io(format!("write assistant-response artifact: {error}")),
                    )
                    .await;
            }
            let artifact_ref = match workspace.seal_artifact(draft).await {
                Ok(reference) => reference,
                Err(error) => {
                    return self
                        .commit_failed(TurnCommitPhase::AssistantMessageArtifact, error)
                        .await;
                }
            };
            if let Some((task_id, turn_id)) = assistant_evidence_identity {
                pending_assistant_evidence = Some(AssistantArtifactEvidence {
                    task_id,
                    turn_id,
                    reference: artifact_ref,
                });
            }
        }
        if let Err(error) = self
            .services
            .context_ingest(ContextIngress::AssistantMessage {
                content: content.clone(),
            })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AssistantMessageIngest, error)
                .await;
        }
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::AssistantMessage { content })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AssistantMessageEvent, error)
                .await;
        }
        let report = match self
            .services
            .context_maintain(ContextMaintenanceTrigger::AfterModel)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                return self
                    .commit_failed(TurnCommitPhase::AfterModelMaintain, error)
                    .await;
            }
        };
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::AfterModel,
                report,
            })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AfterModelMaintainedEvent, error)
                .await;
        }
        // Turn boundary: the full GC pass compacts what the per-event
        // residency machine demoted. Eviction is reversible, and the report
        // explains every eviction and reactivation.
        let report = match self.services.context_gc().await {
            Ok(report) => report,
            Err(error) => {
                return self.commit_failed(TurnCommitPhase::Gc, error).await;
            }
        };
        if let Err(error) = self
            .core
            .emit_event(RuntimeEvent::ContextGc { report })
            .await
        {
            return self.commit_failed(TurnCommitPhase::GcEvent, error).await;
        }
        // 输入记录的 Consumed/Archived 必须在 TurnCompleted 屏障之前入账，
        // 这样 flush 覆盖它们，且 TurnCompleted 仍是屏障前最后一条事件。
        self.emit_input_consumed().await;
        if let Some(applied) = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.applied_input.clone())
        {
            self.emit_input_archived(applied).await;
        }
        // The durability barrier: `emit_event_durable` appends TurnCompleted
        // and then flushes the event journal, so every mandatory state write
        // before it (tool observations, assistant message, maintains, GC)
        // has left the process before the turn is Committed — the channel
        // is FIFO, so the flush covers everything appended before it. A
        // failed barrier means the trace has a gap: the turn is not
        // Committed, and TurnCompleted is never broadcast.
        if let Err(error) = self
            .core
            .emit_event_durable(RuntimeEvent::TurnCompleted)
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::TurnCompletedEvent, error)
                .await;
        }
        if let Some(turn) = self.state.turn.as_mut() {
            turn.turn_state = TurnState::Committed;
        }
        // Publish the evidence locator only after the same durable barrier as
        // the turn. A failed commit must not leave a later `/done` pointing
        // at output from a turn the runtime never declared committed.
        self.state.last_assistant_artifact = pending_assistant_evidence;
        // A `task.complete` proposal must run at the safe point — after the
        // turn is durably committed and no operation is in flight — through
        // the same CTX-10 transaction as `/done`. A completion failure here
        // is surfaced, never allowed to undo the committed turn.
        let pending_completion = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.pending_completion.clone());
        self.state.turn = None;
        if let Some(proposal) = pending_completion {
            self.process_pending_completion(proposal).await;
        }
    }

    /// Run a deferred structured completion proposal after the turn
    /// committed. This is the model-side `task.complete` path: the proposal
    /// becomes the active task's typed CompletionRecord. No active task
    /// (suspended/completed meanwhile) drops the proposal with a warning —
    /// it never fails the already-committed turn.
    async fn process_pending_completion(&mut self, proposal: CompletionProposal) {
        if self.state.tasks.active().is_none() {
            let _ = self
                .core
                .emit_warning("completion proposal dropped: no active task".to_string())
                .await;
            return;
        }
        let Some(next_focus_revision) = self.next_focus_revision().ok() else {
            return;
        };
        if let Err(error) = self
            .commit_completion(proposal.summary, proposal.artifacts, next_focus_revision)
            .await
        {
            let _ = self
                .core
                .emit_warning(format!("completion proposal failed: {error}"))
                .await;
        }
    }

    /// Abort the turn commit: journal the failed phase and the recovery
    /// requirement, then drop the turn frame. No further mandatory writes
    /// happen after a failure — they would build on a state that is already
    /// inconsistent.
    async fn commit_failed(&mut self, phase: TurnCommitPhase, error: AgentError) {
        // A failed mandatory turn write means the runtime can no longer
        // prove that context, the event trace and the caller-visible turn
        // outcome describe the same state. Publishing RecoveryRequired is
        // not itself a fence: persist the poisoned state so every later
        // mutation is rejected until a known-good full restore succeeds.
        self.state.recovery_required = true;
        let _ = self
            .core
            .emit_event(RuntimeEvent::TurnCommitFailed {
                phase: phase.as_str().into(),
                message: error.to_string(),
            })
            .await;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        self.state.turn = None;
    }

    /// Close every tool frame the turn opened (from committed results and
    /// the in-flight op). Called before each model round — the request
    /// consumes the previous results — and on cancellation, so no tool
    /// scope outlives its execution frame. Already-closed scopes are no-ops.
    async fn close_tool_frames(&mut self) -> AgentResult<()> {
        let mut scope_ids: Vec<ScopeId> = Vec::new();
        if let Some(turn) = self.state.turn.as_ref() {
            for step in &turn.turn_frame.steps {
                if let TurnFrameStep::ToolResult {
                    scope_id: Some(id), ..
                } = step
                {
                    scope_ids.push(*id);
                }
            }
            if let Some(op) = turn.op.as_ref()
                && let Some(id) = op.scope_id
            {
                scope_ids.push(id);
            }
        }
        scope_ids.sort_unstable_by_key(|scope_id| scope_id.to_string());
        scope_ids.dedup();
        let total_deadline = tokio::time::Instant::now() + TOOL_SCOPE_CLOSE_TOTAL_TIMEOUT;
        for scope_id in scope_ids {
            let remaining = total_deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(AgentError::RecoveryRequired(
                    "tool-scope cleanup exceeded its total deadline".into(),
                ));
            }
            let timeout = remaining.min(TOOL_SCOPE_CLOSE_TIMEOUT);
            match tokio::time::timeout(timeout, self.services.context_close_scope(scope_id)).await {
                Err(_) => {
                    return Err(AgentError::RecoveryRequired(format!(
                        "closing tool scope {scope_id} exceeded the {timeout:?} deadline"
                    )));
                }
                Ok(result) => match result {
                    Ok(transitions) => {
                        // The close is an auditable result: publish the
                        // lifecycle transitions it produced (a tool frame's
                        // durable outcomes promoted out of the frame). An empty
                        // transition list is a no-op close — nothing to report.
                        if !transitions.is_empty() {
                            let _ = self
                                .core
                                .emit_event(RuntimeEvent::ToolScopeClosed {
                                    scope_id,
                                    transitions,
                                })
                                .await;
                        }
                    }
                    Err(error) => {
                        return Err(AgentError::RecoveryRequired(format!(
                            "closing tool scope {scope_id} failed: {error}"
                        )));
                    }
                },
            }
        }
        Ok(())
    }

    /// Cancel the active turn and durably publish its distinct terminal
    /// state. Cancellation is effective before the barrier (the operation
    /// is fenced and its late completion is stale), but a failed barrier
    /// returns `RecoveryRequired` and poisons ordinary mutation rather than
    /// pretending the cancellation was durably acknowledged.
    async fn cancel_turn(
        &mut self,
        reason: TurnCancellationReason,
        operation_id_override: Option<OperationId>,
    ) -> AgentResult<TurnCancelAck> {
        let Some(turn) = self.state.turn.as_ref() else {
            return Ok(TurnCancelAck::NoActiveTurn);
        };
        let cancelled_generation = self.state.generation;
        let operation_id = turn
            .op
            .as_ref()
            .map(|operation| operation.operation_id)
            .or(operation_id_override);
        let cleanup_kind = turn.op.as_ref().map(|operation| operation.kind);
        // Install the Core-owned fence before any await or cleanup. A tool
        // completion racing cancellation can no longer commit an effect
        // while scope closure is blocked.
        let effective_generation = self.bump_generation()?;
        let tool_identity = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.op.as_ref())
            .and_then(|operation| operation.tool_identity.clone());
        if let Some(operation) = self.state.turn.as_ref().and_then(|turn| turn.op.as_ref()) {
            operation.cancel.cancel();
        }
        if let Some(identity) = tool_identity
            && let Err(error) = self.core.cancel_operation(identity)
        {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(AgentError::RecoveryRequired(format!(
                "Core could not install the cancelled operation terminal: {error}"
            )));
        }
        if cleanup_kind == Some(OpKind::Tool) {
            self.state.pending_tool_cleanup = operation_id;
        }
        if let Err(error) = self.close_tool_frames().await {
            self.state.recovery_required = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(error.to_string()),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(error);
        }
        let mut turn = self
            .state
            .turn
            .take()
            .expect("the active turn was inspected immediately above");
        turn.op = None;
        let event = RuntimeEvent::TurnCancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
            reason,
        };
        if let Err(error) = self.core.emit_event_durable(event).await {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            return Err(AgentError::RecoveryRequired(format!(
                "turn {} was cancelled, but its audit barrier failed: {error}",
                turn.turn_id
            )));
        }
        self.publish_interrupt_committed(
            turn.turn_id,
            turn.applied_input.as_ref().and_then(|input| input.input_id),
            reason,
        )
        .await;
        Ok(TurnCancelAck::Cancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
        })
    }

    /// Complete the actor-owned cleanup and durable cancellation event after
    /// Core has atomically installed both the new epoch and (for a tool)
    /// cancellation terminal.
    async fn finish_cancelled_turn(
        &mut self,
        reason: TurnCancellationReason,
        operation_id_override: Option<OperationId>,
        cancelled_generation: u64,
        effective_generation: u64,
    ) -> AgentResult<TurnCancelAck> {
        self.state.generation = effective_generation;
        let Some(turn) = self.state.turn.as_ref() else {
            return self
                .operation_control_recovery(
                    "Core installed an operation cancellation after the active turn disappeared"
                        .into(),
                )
                .await;
        };
        let operation_id = turn
            .op
            .as_ref()
            .map(|operation| operation.operation_id)
            .or(operation_id_override);
        let cleanup_kind = turn.op.as_ref().map(|operation| operation.kind);
        if let Some(operation) = turn.op.as_ref() {
            operation.cancel.cancel();
        }
        if cleanup_kind == Some(OpKind::Tool) {
            self.state.pending_tool_cleanup = operation_id;
        }
        if let Err(error) = self.close_tool_frames().await {
            self.state.recovery_required = true;
            let _ = self
                .core
                .emit_event(RuntimeEvent::Error {
                    message: crate::output::bound_error_message(error.to_string()),
                })
                .await;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            self.state.turn = None;
            return Err(error);
        }
        let mut turn = self
            .state
            .turn
            .take()
            .expect("the active turn was inspected immediately above");
        turn.op = None;
        let event = RuntimeEvent::TurnCancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
            reason,
        };
        if let Err(error) = self.core.emit_event_durable(event).await {
            self.state.recovery_required = true;
            let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
            return Err(AgentError::RecoveryRequired(format!(
                "turn {} was cancelled, but its audit barrier failed: {error}",
                turn.turn_id
            )));
        }
        self.publish_interrupt_committed(
            turn.turn_id,
            turn.applied_input.as_ref().and_then(|input| input.input_id),
            reason,
        )
        .await;
        Ok(TurnCancelAck::Cancelled {
            turn_id: turn.turn_id,
            task_id: self.state.task_id,
            operation_id,
            cancelled_generation,
            effective_generation,
        })
    }

    /// Cancel exactly the active tool operation, then return Core's durable
    /// post-cancellation truth. The complete identity comparison is the
    /// trusted-boundary canonicalization step: a caller cannot retarget a
    /// current turn by supplying only a matching operation id.
    async fn cancel_operation(
        &mut self,
        identity: ToolOperationIdentity,
    ) -> AgentResult<OperationQueryResult> {
        identity.validate().map_err(AgentError::InvalidRequest)?;
        if self.state.recovery_required {
            return Err(AgentError::RecoveryRequired(
                "runtime recovery is required before operation cancellation may continue".into(),
            ));
        }
        if let AuthorityRecoveryStatus::RecoveryRequired { reason } = self.core.recovery_status() {
            return self
                .operation_control_recovery(format!(
                    "Core authority recovery is required before operation cancellation: {reason}"
                ))
                .await;
        }

        let active_identity = self
            .state
            .turn
            .as_ref()
            .and_then(|turn| turn.op.as_ref())
            .filter(|operation| operation.kind == OpKind::Tool)
            .and_then(|operation| operation.tool_identity.as_ref())
            .ok_or_else(|| {
                AgentError::InvalidRequest(
                    "operation cancellation requires a current in-flight tool operation".into(),
                )
            })?;
        if active_identity != &identity {
            return Err(AgentError::InvalidRequest(
                "operation identity does not match the current in-flight tool operation".into(),
            ));
        }

        let cancelled_generation = self.state.generation;
        let disposition = match self
            .core
            .cancel_operation_and_advance(identity.clone(), cancelled_generation)
        {
            Ok(disposition) => disposition,
            Err(AgentError::RecoveryRequired(reason)) => {
                return self
                    .operation_control_recovery(format!(
                        "Core could not durably cancel operation {}: {reason}",
                        identity.operation_id
                    ))
                    .await;
            }
            Err(error) => return Err(error),
        };
        let (effective_generation, result) = match disposition {
            OperationCancelDisposition::AlreadySettled(result) => {
                if matches!(result, OperationQueryResult::ExpiredOrPossiblySeen) {
                    return self
                        .operation_control_recovery(format!(
                            "operation {} is expired or only conservatively known; cancellation cannot infer its state",
                            identity.operation_id
                        ))
                        .await;
                }
                return Ok(result);
            }
            OperationCancelDisposition::Cancelled {
                effective_epoch,
                result,
            } => (effective_epoch, result),
        };
        let acknowledgement = self
            .finish_cancelled_turn(
                TurnCancellationReason::Requested,
                Some(identity.operation_id),
                cancelled_generation,
                effective_generation,
            )
            .await?;
        if !matches!(
            acknowledgement,
            TurnCancelAck::Cancelled {
                operation_id: Some(operation_id),
                ..
            } if operation_id == identity.operation_id
        ) {
            return self
                .operation_control_recovery(format!(
                    "operation {} cancellation did not produce its durable turn acknowledgement",
                    identity.operation_id,
                ))
                .await;
        }

        let cancelled = matches!(
            &result,
            OperationQueryResult::Found { snapshot }
                if snapshot.identity == identity
                    && matches!(
                        snapshot.state,
                        OperationState::Terminal {
                            terminal: OperationTerminal::CancelledBeforeCommit,
                            ..
                        }
                    )
        );
        if cancelled {
            Ok(result)
        } else {
            self.operation_control_recovery(format!(
                "operation {} passed the cancellation barrier without a matching Core terminal",
                identity.operation_id,
            ))
            .await
        }
    }

    async fn operation_control_recovery<T>(&mut self, message: String) -> AgentResult<T> {
        self.state.recovery_required = true;
        let _ = self.core.emit_event(RuntimeEvent::RecoveryRequired).await;
        Err(AgentError::RecoveryRequired(format!(
            "{message}; runtime remains fenced"
        )))
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

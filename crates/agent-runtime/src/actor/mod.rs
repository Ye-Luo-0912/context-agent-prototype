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

mod commands;
mod lifecycle;
mod model;
mod restore;
#[cfg(test)]
mod restore_tests;
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

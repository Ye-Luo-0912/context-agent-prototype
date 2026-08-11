//! The runtime actor: owns the mutable runtime state and drives the turn
//! execution state machine. Model rounds and tool calls are *operations*:
//! they execute as spawned tasks and report an `OperationResult`; the actor
//! validates the result against the current generation and only then commits
//! it (context ingest/maintenance, turn-frame pushes, events). Stale results
//! are dropped — they never race into the new state.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentError, AgentResult, CONTEXT_CONSUMPTION_ACK_ITEM_CAP, CancellationToken,
    ContextConsumptionAck, ContextHints, ContextIngress, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextRetention, Effect, EffectCommitError,
    ModelInput, ModelRequest, OperationId, OperationOutcome, OperationResult, RestoreRevision,
    RuntimeDirective, RuntimeEvent, ScopeId, ScopeKind, TaskId, ToolCall, ToolOutcome, ToolOutput,
    ToolResultDisposition, ToolSurfaceBlock, ToolSurfaceBlockReason, ToolSurfaceDemand,
    ToolSurfaceSnapshot, TurnFrame, TurnFrameStep, TurnId,
};
use agent_kernel::AgentKernel;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::budget::{
    DEFAULT_OUTPUT_RESERVE, MAX_TOOL_SURFACE_TOKENS, ModelBudget, approx_layer_tokens,
};
use crate::checkpoint::{
    RUNTIME_CHECKPOINT_VERSION, RunMetadata, RuntimeCheckpoint, TaskManagerSnapshot,
};
use crate::command::{Reply, RuntimeCommand, RuntimeHandle};
use crate::output::bound_tool_output;
use crate::prompt::PromptAssembler;
use crate::sink::LiveSink;
use crate::surface::{RoundSurfacePlan, SurfaceReportContext};
use crate::task::{TaskManager, normalize_tool_requirements};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Which operation a spawned task is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Model,
    Tool,
}

/// Identity of one in-flight operation, captured when it is spawned so its
/// late completion can be validated before any commit.
struct InFlightOp {
    operation_id: OperationId,
    turn_id: TurnId,
    generation: u64,
    /// The tool scope opened for this operation (tool ops only).
    scope_id: Option<ScopeId>,
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
    /// A context transaction failed and its checkpoint rollback also failed.
    /// The two authority planes can no longer be proven aligned, so no
    /// further mutation is accepted until the process is recovered from a
    /// known-good RuntimeCheckpoint.
    recovery_required: bool,
    /// The runtime's view of the current scope (filled once the context
    /// engine exposes its scope tree through the contract).
    scope_id: Option<ScopeId>,
    turn: Option<ActiveTurn>,
}

pub struct RuntimeActor {
    kernel: Arc<AgentKernel>,
    /// Owns the system prompt and renders the five-layer model input. The
    /// context engine only ever returns structured items.
    assembler: PromptAssembler,
    state: ActorState,
}

impl RuntimeActor {
    pub fn new(kernel: Arc<AgentKernel>) -> Self {
        Self {
            assembler: PromptAssembler::new(kernel.system_prompt()),
            kernel,
            state: ActorState::default(),
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
                            let result = self.shutdown().await;
                            let _ = reply.send(result);
                            return;
                        }
                        Some(command) => self.process(command, &op_tx).await,
                        // Every caller handle was dropped: the run must still
                        // shut down cleanly (cancel, flush, RunCompleted)
                        // instead of silently returning.
                        None => {
                            let _ = self.shutdown().await;
                            return;
                        }
                    }
                }
                completed = op_rx.recv() => {
                    match completed {
                        Some(completion) => self.on_operation_completed(completion, &op_tx).await,
                        None => {
                            let _ = self.shutdown().await;
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
    async fn shutdown(&mut self) -> AgentResult<()> {
        self.cancel_turn().await;
        self.kernel.stop().await
    }

    async fn process(
        &mut self,
        command: RuntimeCommand,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        match command {
            RuntimeCommand::Start { reply } => {
                let _ = reply.send(self.kernel.start().await);
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
                        match self.kernel.set_focus(task_id, goal).await {
                            Ok(report) => {
                                self.state.tasks.commit(txn);
                                self.state.task_id = Some(task_id);
                                self.state
                                    .task_requirement_high_water
                                    .entry(task_id)
                                    .or_insert(0);
                                self.state.focus_revision = next_focus_revision;
                                self.state.generation += 1;
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
                            match self.kernel.set_focus(task_id, goal).await {
                                Ok(report) => {
                                    self.state.tasks.commit(txn);
                                    self.state.task_id = Some(task_id);
                                    self.state
                                        .task_requirement_high_water
                                        .entry(task_id)
                                        .or_insert(0);
                                    self.state.focus_revision = next_focus_revision;
                                    self.state.generation += 1;
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
                        Some(txn) => match self.kernel.clear_focus().await {
                            Ok(report) => {
                                self.state.tasks.commit(txn);
                                self.state.task_id = None;
                                self.state.focus_revision = next_focus_revision;
                                self.state.generation += 1;
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
                                        match self
                                            .kernel
                                            .emit_event(RuntimeEvent::TaskToolRequirementsChanged {
                                                task_id,
                                                revision,
                                                requirements: entries,
                                            })
                                            .await
                                        {
                                            Err(error) => Err(error),
                                            Ok(()) => {
                                                self.state.tasks.commit(txn);
                                                self.state
                                                    .task_requirement_high_water
                                                    .insert(task_id, revision);
                                                self.state.generation += 1;
                                                Ok(revision)
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
                                match self
                                    .kernel
                                    .emit_event(RuntimeEvent::TaskAnchorChanged {
                                        task_id,
                                        revision,
                                        changed_fields,
                                    })
                                    .await
                                {
                                    Err(error) => Err(error),
                                    Ok(()) => {
                                        self.state.tasks.commit(txn);
                                        self.state.generation += 1;
                                        Ok(revision)
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
                        match self.kernel.pin(content).await {
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
                        // The exact final-output body is the completion
                        // summary itself in this prototype: retain its
                        // digest so the outcome stays byte-for-byte
                        // verifiable, with a deterministic ref naming the
                        // task's completion record.
                        let final_output_digest = Some(crate::task::sha256_hex(summary.as_bytes()));
                        let final_output_ref = self
                            .state
                            .tasks
                            .active()
                            .map(|task_id| format!("task:{task_id}:completion"));
                        match self.state.tasks.prepare_complete(
                            summary.clone(),
                            final_output_ref,
                            final_output_digest,
                        ) {
                            None => Err(AgentError::InvalidRequest(
                                "no active task to complete".into(),
                            )),
                            Some((txn, record)) => {
                                let task_id = record.task_id;
                                let anchor_revision = record.anchor_revision;
                                let event_summary = record.summary.clone();
                                match self.kernel.complete_current_task(task_id, summary).await {
                                    Ok(report) => {
                                        self.state.tasks.commit(txn);
                                        self.state.task_id = None;
                                        self.state.focus_revision = next_focus_revision;
                                        self.state.generation += 1;
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
                                        // The completed task's working set is
                                        // outside the runtime now: run one
                                        // full GC pass so its records leave
                                        // the resident heap (they stay
                                        // recallable from the buffer/store —
                                        // durable retention, storage GC
                                        // protected). A GC failure after the
                                        // completion committed is surfaced,
                                        // never allowed to undo the outcome.
                                        if transition.is_ok() {
                                            self.compact_after_completion().await;
                                        }
                                        transition
                                    }
                                    Err(error) => Err(self.context_transition_failed(error)),
                                }
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Checkpoint { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => match self.kernel.checkpoint().await {
                        Ok(context) => Ok(RuntimeCheckpoint {
                            version: RUNTIME_CHECKPOINT_VERSION,
                            run_metadata: RunMetadata {
                                run_id: self.kernel.run_id(),
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
                        }),
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Restore { checkpoint, reply } => {
                // Restore is the one mutation allowed while poisoned: it is
                // how a known-good full checkpoint re-establishes authority.
                let result = match self.ensure_no_active_turn() {
                    Ok(()) => {
                        match checkpoint.validate() {
                            Err(error) => Err(error),
                            Ok(()) => {
                                // Restore may load an older checkpoint into a
                                // still-running actor. Treat the restored
                                // focus as a new epoch so source revisions
                                // never move backwards or alias a surface
                                // prepared before the restore.
                                let restored_focus_revision = match self
                                    .state
                                    .focus_revision
                                    .max(checkpoint.focus_revision)
                                    .checked_add(1)
                                {
                                    Some(revision) => revision,
                                    None => {
                                        let _ = reply.send(Err(AgentError::Internal(
                                            "runtime focus revision is exhausted".into(),
                                        )));
                                        return;
                                    }
                                };
                                let RuntimeCheckpoint {
                                    mut tasks,
                                    current_task_id,
                                    focus_revision,
                                    last_surface_revision,
                                    context,
                                    capabilities,
                                    run_metadata,
                                    version,
                                    ..
                                } = checkpoint;
                                let mut restored_requirement_high_water =
                                    self.state.task_requirement_high_water.clone();
                                for task in self.state.tasks.list_records() {
                                    restored_requirement_high_water
                                        .entry(task.id)
                                        .and_modify(|revision| {
                                            *revision =
                                                (*revision).max(task.tool_requirements.revision);
                                        })
                                        .or_insert(task.tool_requirements.revision);
                                }
                                // Rebase bookkeeping for the restore-commit
                                // audit record: which tasks had their
                                // tool-requirement revision bumped past the
                                // live high-water mark (capped sample).
                                let mut rebased_tasks = 0usize;
                                let mut rebased_task_sample: Vec<TaskId> = Vec::new();
                                for task in &mut tasks.tasks {
                                    if let Some(live_revision) =
                                        restored_requirement_high_water.get(&task.id).copied()
                                        && live_revision >= task.tool_requirements.revision
                                    {
                                        task.tool_requirements.revision = match live_revision
                                            .checked_add(1)
                                        {
                                            Some(revision) => revision,
                                            None => {
                                                let _ = reply.send(Err(
                                                        AgentError::Internal(format!(
                                                            "task {} tool-requirement revision is exhausted",
                                                            task.id
                                                        )),
                                                    ));
                                                return;
                                            }
                                        };
                                        rebased_tasks += 1;
                                        if rebased_task_sample.len() < 16 {
                                            rebased_task_sample.push(task.id);
                                        }
                                    }
                                    restored_requirement_high_water
                                        .insert(task.id, task.tool_requirements.revision);
                                }
                                let old_focus_revision = self.state.focus_revision;
                                let old_surface_revision = self.state.last_surface_revision;
                                match self.kernel.restore(context, current_task_id).await {
                                    Err(error) => Err(self.context_transition_failed(error)),
                                    Ok(()) => {
                                        // No fallible operation follows this
                                        // point: context and task authority
                                        // become visible together.
                                        self.state.tasks.restore(tasks);
                                        self.state.task_id = current_task_id;
                                        self.state.task_requirement_high_water =
                                            restored_requirement_high_water;
                                        self.state.focus_revision = restored_focus_revision;
                                        self.state.last_surface_revision = self
                                            .state
                                            .last_surface_revision
                                            .max(last_surface_revision);
                                        self.state.generation += 1;
                                        self.state.recovery_required = false;
                                        // Mandatory audit record of the
                                        // restore commit. A restore must not
                                        // outrun its own journal event: if
                                        // the barrier fails, the restored
                                        // state stays (it is the aligned
                                        // truth) but the runtime demands
                                        // recovery and rejects normal
                                        // mutation until a known-good
                                        // restore.
                                        let restored_event = RuntimeEvent::RuntimeRestored {
                                            checkpoint_version: version,
                                            restored_run_id: run_metadata.run_id,
                                            current_run_id: self.kernel.run_id(),
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
                                            capabilities_applied: !capabilities.is_empty(),
                                        };
                                        match self.kernel.emit_event_durable(restored_event).await {
                                            Ok(()) => Ok(()),
                                            Err(error) => {
                                                self.state.recovery_required = true;
                                                let _ = self
                                                    .kernel
                                                    .emit_event(RuntimeEvent::RecoveryRequired)
                                                    .await;
                                                Err(error)
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::EmitDiagnostics { reply } => {
                // Pure read of engine state: allowed at any time.
                let _ = reply.send(self.kernel.emit_diagnostics().await);
            }
            RuntimeCommand::InspectContext { limit, reply } => {
                let _ = reply.send(self.kernel.inspect_context(limit).await);
            }
            RuntimeCommand::CancelTurn { reply } => {
                self.cancel_turn().await;
                let _ = reply.send(());
            }
            RuntimeCommand::Stop { .. } => unreachable!("Stop is handled in the run loop"),
        }
    }

    /// A turn is accepted only when the runtime is idle. Serializing every
    /// mutation removes the structural race where focus/pin/task commands
    /// interleaved with an in-flight turn.
    fn ensure_idle(&self) -> AgentResult<()> {
        if self.state.recovery_required {
            Err(AgentError::RecoveryRequired(
                "runtime recovery is required after a context transaction rollback failed".into(),
            ))
        } else {
            self.ensure_no_active_turn()
        }
    }

    fn next_focus_revision(&self) -> AgentResult<u64> {
        self.state
            .focus_revision
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("runtime focus revision is exhausted".into()))
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
            let kernel = self.kernel.clone();
            tokio::spawn(async move {
                let _ = kernel.emit_event(RuntimeEvent::RecoveryRequired).await;
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
        if let Err(error) = self.kernel.emit_event(event).await {
            return Err(self.audit_gap_after_commit(error).await);
        }
        if let Err(error) = self
            .kernel
            .emit_event(RuntimeEvent::ContextMaintained { trigger, report })
            .await
        {
            return Err(self.audit_gap_after_commit(error).await);
        }
        Ok(())
    }

    async fn audit_gap_after_commit(&mut self, error: AgentError) -> AgentError {
        self.state.recovery_required = true;
        let _ = self.kernel.emit_event(RuntimeEvent::RecoveryRequired).await;
        AgentError::RecoveryRequired(format!(
            "context/task transition committed, but its audit event failed ({error})"
        ))
    }

    /// One full GC pass after a task completed, so the finished task's
    /// records leave the resident heap and stay recallable from the
    /// reversible buffer / context store. The completion itself is already
    /// committed; a GC failure is surfaced as an `Error` event and never
    /// rolls the outcome back.
    async fn compact_after_completion(&mut self) {
        match self.kernel.context_gc().await {
            Ok(report) => {
                if let Err(error) = self
                    .kernel
                    .emit_event(RuntimeEvent::ContextGc { report })
                    .await
                {
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                }
            }
            Err(error) => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::Error {
                        message: format!("post-completion GC failed: {error}"),
                    })
                    .await;
            }
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
        if let Err(error) = self.ensure_idle() {
            let _ = reply.send(Err(error));
            return;
        }
        if content.trim().is_empty() {
            let _ = reply.send(Ok(()));
            return;
        }

        // The first message with no active task auto-creates one: a task is
        // the long-lived entity and the engine must never mint a TaskId, so
        // this is the single place an implicit task can be born. The focus
        // change lands before the message is ingested, exactly like an
        // explicit `/focus`.
        if self.state.tasks.active().is_none() {
            let next_focus_revision = match self.next_focus_revision() {
                Ok(revision) => revision,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let (txn, task_id) = self.state.tasks.prepare_create(content.trim());
            match self.kernel.set_focus(task_id, content.clone()).await {
                Err(error) => {
                    let error = self.context_transition_failed(error);
                    let _ = reply.send(Err(error));
                    return;
                }
                Ok(report) => {
                    self.state.tasks.commit(txn);
                    self.state.task_id = Some(task_id);
                    self.state
                        .task_requirement_high_water
                        .entry(task_id)
                        .or_insert(0);
                    self.state.focus_revision = next_focus_revision;
                    if let Err(error) = self
                        .publish_context_transition(
                            RuntimeEvent::FocusChanged {
                                task_id,
                                goal: content.clone(),
                            },
                            ContextMaintenanceTrigger::FocusChanged,
                            report,
                        )
                        .await
                    {
                        let _ = reply.send(Err(error));
                        return;
                    }
                }
            }
        }

        self.state.generation += 1;
        let turn_id = TurnId::new();
        if let Err(error) = self
            .kernel
            .emit_event(RuntimeEvent::UserMessageAccepted {
                content: content.clone(),
            })
            .await
        {
            let _ = reply.send(Err(error));
            return;
        }
        if let Err(error) = self
            .kernel
            .context_ingest(ContextIngress::UserMessage {
                content: content.clone(),
            })
            .await
        {
            let _ = reply.send(Err(error));
            return;
        }
        match self
            .kernel
            .context_maintain(ContextMaintenanceTrigger::UserInput)
            .await
        {
            Ok(report) => {
                if let Err(error) = self
                    .kernel
                    .emit_event(RuntimeEvent::ContextMaintained {
                        trigger: ContextMaintenanceTrigger::UserInput,
                        report,
                    })
                    .await
                {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        }

        self.state.turn = Some(ActiveTurn {
            turn_id,
            turn_frame: TurnFrame::new(content),
            model_round: 0,
            pending_tools: VecDeque::new(),
            tool_surface: None,
            turn_state: TurnState::Running,
            op: None,
        });
        self.advance_turn(op_tx).await;
        let _ = reply.send(Ok(()));
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
            Action::Model => {
                let over_budget = self.state.turn.as_ref().is_some_and(|turn| {
                    turn.op.is_none() && turn.model_round >= self.kernel.max_tool_rounds()
                });
                if over_budget {
                    let message = format!(
                        "tool round budget exhausted after {} rounds",
                        self.kernel.max_tool_rounds()
                    );
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::Warning {
                            message: message.clone(),
                        })
                        .await;
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::Error { message })
                        .await;
                    self.state.turn = None;
                    return;
                }
                self.spawn_model_operation(op_tx).await;
            }
            Action::Tool(call) => self.spawn_tool_operation(call, op_tx).await,
        }
    }

    /// Prepare + spawn one model round: close the consumed tool frames,
    /// maintenance, materialize, assemble, then the model call as an
    /// operation.
    async fn spawn_model_operation(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
        // The previous round's tool frames end here: the model request below
        // consumes their results (they ride in the turn frame).
        self.close_tool_frames().await;

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
            .kernel
            .context_maintain(ContextMaintenanceTrigger::BeforeModel)
            .await
        {
            Ok(report) => {
                if let Err(error) = self
                    .kernel
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
                        .kernel
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
                    .kernel
                    .emit_event(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
        }

        // Tool lifecycle safe point. Task demand is declarative only: reload
        // can restore catalog/schema readiness, but cannot enable a disabled
        // capability, grant a permission or bypass approval/effect policy.
        self.kernel.tool_gc();
        let (task_requirement_revision, requirements) = self
            .state
            .task_id
            .and_then(|task_id| self.state.tasks.get(task_id))
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
            .kernel
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        visible_names.extend(
            self.kernel
                .tool_catalog()
                .into_iter()
                .filter(|entry| entry.state.in_surface())
                .map(|entry| entry.name),
        );
        for requirement in &requirements {
            if !visible_names.contains(&requirement.tool_name) {
                let _ = self.kernel.tool_load(&requirement.tool_name);
            }
        }

        // Dispatcher snapshot is the complete currently-loaded candidate
        // set. Runtime owns the sole bounded projection so Task MustSurface
        // can never disappear inside a provider adapter before policy sees it.
        let candidates = self.kernel.tool_snapshot();
        let candidate_names: HashSet<_> = candidates
            .specs
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
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
            self.kernel.tool_may_omit_from_round(name)
        });
        surface_plan
            .source_revisions_mut()
            .task_requirement_revision = task_requirement_revision;
        surface_plan.source_revisions_mut().focus_revision =
            self.state.task_id.map(|_| self.state.focus_revision);
        for requirement in &unavailable_optional {
            surface_plan.add_unavailable(requirement);
        }

        if !unavailable_must.is_empty() {
            let surface_revision = match self.issue_surface_revision() {
                Ok(revision) => revision,
                Err(error) => {
                    let _ = self
                        .kernel
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
                .kernel
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .kernel
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
                .kernel
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
                        .kernel
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
                .kernel
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .kernel
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
                .kernel
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

        // The engine only ever sees its own slice of the provider window:
        // the output reserve, system policy, turn frame and active tool
        // schemas are the runtime's share and are subtracted before the
        // engine budgets the working set.
        let capabilities = self.kernel.model_capabilities();
        let turn_frame_tokens = approx_layer_tokens(&turn_frame.messages());
        let active_tools_tokens = approx_layer_tokens(&surface_plan.specs());
        let provider_window = capabilities
            .context_window
            .unwrap_or_else(|| self.kernel.context_budget_tokens());
        // The output reserve is a hard subtraction: the answer must always
        // have room, and rendering overhead must never eat into it.
        let output_reserve = if capabilities.max_output_tokens > 0 {
            capabilities.max_output_tokens
        } else {
            DEFAULT_OUTPUT_RESERVE
        };
        let model_budget = ModelBudget::compute(
            provider_window,
            output_reserve,
            self.assembler.system_prompt_tokens(),
            turn_frame_tokens,
            active_tools_tokens,
        );
        let materialized = match self
            .kernel
            .context_materialize(ContextQuery {
                current_input: current_input.clone(),
                budget_tokens: model_budget.context_frame_budget,
                hints: ContextHints {
                    max_selected_items: Some(CONTEXT_CONSUMPTION_ACK_ITEM_CAP),
                },
            })
            .await
        {
            Ok(materialized) => materialized,
            Err(error) => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
        };
        // Runtime final guard: the engine priced the working-set content,
        // but the assembler's rendering overhead (section headers, per-item
        // frame labels) is the runtime's share. The assembled request must
        // fit the *input* budget — the window minus the output reserve —
        // because the answer must always have room. Trim the context frame
        // until it fits; if the fixed layers alone (system + turn + tools)
        // still overshoot, omit optional schemas from this round snapshot;
        // a request whose mandatory fixed layers still do not fit is a hard
        // error, never a lifecycle mutation or silently over-budget send.
        let max_input_budget = provider_window.saturating_sub(output_reserve);
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
                    .kernel
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
            .kernel
            .emit_event(RuntimeEvent::ContextPrepared {
                diagnostics: materialized.diagnostics.clone(),
                selected: materialized.selected.clone(),
            })
            .await
        {
            let _ = self
                .kernel
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
                .kernel
                .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
                .await
            {
                let _ = self
                    .kernel
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
                .kernel
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
            scope_id: None,
            cancel: cancel.clone(),
        });

        if let Err(error) = self
            .kernel
            .emit_event(RuntimeEvent::ToolSurfacePlanned { report })
            .await
        {
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
            self.state.turn = None;
            return;
        }
        if let Err(error) = self
            .kernel
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
                .kernel
                .emit_event(RuntimeEvent::Error {
                    message: error.to_string(),
                })
                .await;
            self.state.turn = None;
            return;
        }

        let kernel = self.kernel.clone();
        let sink = LiveSink::new(
            kernel.event_sender(),
            kernel.seq(),
            kernel.run_id(),
            turn_id,
            operation_id,
            generation,
        );
        let op_tx = op_tx.clone();
        let run_id = kernel.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        tokio::spawn(async move {
            let outcome = match kernel
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
                    directive: None,
                    disposition: ToolResultDisposition::PersistObservation,
                    context_ack: Some(context_ack),
                })
                .await;
        });
    }

    /// Prepare + spawn one tool call: open the tool's execution frame, emit
    /// ToolStarted, then run approval + dispatch as an operation.
    async fn spawn_tool_operation(
        &mut self,
        call: ToolCall,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
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
        let tool_scope = match self.kernel.context_open_scope(ScopeKind::Tool, None).await {
            Ok(scope) => Some(scope),
            Err(error) => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.state.turn = None;
                return;
            }
        };
        let _ = self
            .kernel
            .emit_event(RuntimeEvent::ToolStarted { call: call.clone() })
            .await;

        let cancel = CancellationToken::new();
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            scope_id: tool_scope,
            cancel: cancel.clone(),
        });

        let kernel = self.kernel.clone();
        let op_tx = op_tx.clone();
        let run_id = kernel.run_id();
        let task_id = self.state.task_id;
        tokio::spawn(async move {
            let outcome = kernel.execute_tool(call, cancel, &surface).await;
            let (operation, effect, directive, disposition) = match outcome {
                ToolOutcome::Value(output) => (
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
                    ToolResultDisposition::PersistObservation,
                ),
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
                    let resolved = kernel.resolve_engine_query(output, query).await;
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
                    directive,
                    disposition,
                    context_ack: None,
                })
                .await;
        });
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
                effect.rollback(&reason).await;
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
            if let Err(error) = self.kernel.emit_warning(message).await {
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
                    && let Err(error) = self.kernel.acknowledge_context_consumption(ack).await
                {
                    let error = self.context_transition_failed(error);
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::Error {
                            message: format!("failed to commit model context consumption: {error}"),
                        })
                        .await;
                    self.state.turn = None;
                    return;
                }
                // Report the round's true provider usage to live consumers
                // (the eval harness, a token meter). Best-effort: a journal
                // failure here must not abort the turn commit.
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::ModelUsed {
                        input_tokens: usage.input_tokens.unwrap_or(0),
                        output_tokens: usage.output_tokens.unwrap_or(0),
                    })
                    .await;
                if tool_calls.is_empty() {
                    self.finalize_turn(content).await;
                } else {
                    if let Some(turn) = self.state.turn.as_mut() {
                        turn.turn_frame.push_tool_calls(tool_calls.clone());
                        turn.pending_tools.extend(tool_calls);
                    }
                    self.advance_turn(op_tx).await;
                }
            }
            OperationOutcome::ToolOutput(output) => {
                // The generation fence passed: commit the staged effect
                // before the result enters the turn frame. A commit failure
                // is surfaced as a failed tool result — the model must not
                // see "edit applied" when the rename never landed.
                let output = match completion.effect {
                    Some(effect) => match effect.commit().await {
                        Ok(()) => output,
                        Err(EffectCommitError::NotApplied(error)) => ToolOutput {
                            ok: false,
                            summary: format!("effect commit failed: {error}"),
                            model_content: format!(
                                "the change was prepared but could not be committed: {error}"
                            ),
                            ..output
                        },
                        Err(EffectCommitError::AppliedButDurabilityFailed(error)) => {
                            // The side effect landed but its journal record
                            // did not: the world and the journal disagree.
                            // The model must be told the truth — "applied but
                            // not recorded" — and the runtime flags a
                            // degraded/recovery state instead of pretending
                            // nothing happened.
                            let _ = self
                                .kernel
                                .emit_warning(format!(
                                    "effect applied but its journal record failed: {error}"
                                ))
                                .await;
                            ToolOutput {
                                ok: false,
                                summary: format!(
                                    "effect applied but its journal record failed: {error}"
                                ),
                                model_content: format!(
                                    "the change WAS applied to the file, but recording it in the change journal failed: {error}. The filesystem and the journal now disagree — recovery is required."
                                ),
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
                    .kernel
                    .context_ingest(ContextIngress::WorkingSetSignal {
                        content: output.model_content.clone(),
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
                    .kernel
                    .emit_event(RuntimeEvent::ToolFinished { output })
                    .await;
                self.advance_turn(op_tx).await;
            }
            OperationOutcome::Failed { message } => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::Error { message })
                    .await;
                self.state.turn = None;
            }
            OperationOutcome::Cancelled => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::Warning {
                        message: "turn cancelled".into(),
                    })
                    .await;
                let _ = self.kernel.emit_event(RuntimeEvent::TurnCompleted).await;
                self.state.turn = None;
            }
            OperationOutcome::Completed => {
                self.state.turn = None;
            }
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
                match self.kernel.context_gc().await {
                    Ok(report) => {
                        if let Err(error) = self
                            .kernel
                            .emit_event(RuntimeEvent::ContextGc { report })
                            .await
                        {
                            // The GC state change landed but its audit
                            // event did not: surface it instead of letting
                            // the state silently outrun its journal event.
                            let _ = self
                                .kernel
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
                            .kernel
                            .emit_event(RuntimeEvent::Error {
                                message: error.to_string(),
                            })
                            .await;
                    }
                }
            }
            RuntimeDirective::Context(other) => {
                if let Err(error) = self
                    .kernel
                    .context_ingest(ContextIngress::ContextDirective { action: other })
                    .await
                {
                    // A quota refused the directive (keep_alive / lease
                    // caps): the model believes it was granted, so surface
                    // the refusal.
                    let _ = self
                        .kernel
                        .emit_warning(format!("context directive refused: {error}"))
                        .await;
                }
            }
        }
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
                    .kernel
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
                .kernel
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
                .kernel
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
        if let Err(error) = self
            .kernel
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
            .kernel
            .emit_event(RuntimeEvent::AssistantMessage { content })
            .await
        {
            return self
                .commit_failed(TurnCommitPhase::AssistantMessageEvent, error)
                .await;
        }
        let report = match self
            .kernel
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
            .kernel
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
        let report = match self.kernel.context_gc().await {
            Ok(report) => report,
            Err(error) => {
                return self.commit_failed(TurnCommitPhase::Gc, error).await;
            }
        };
        if let Err(error) = self
            .kernel
            .emit_event(RuntimeEvent::ContextGc { report })
            .await
        {
            return self.commit_failed(TurnCommitPhase::GcEvent, error).await;
        }
        // The durability barrier: `emit_event_durable` appends TurnCompleted
        // and then flushes the event journal, so every mandatory state write
        // before it (tool observations, assistant message, maintains, GC)
        // has left the process before the turn is Committed — the channel
        // is FIFO, so the flush covers everything appended before it. A
        // failed barrier means the trace has a gap: the turn is not
        // Committed, and TurnCompleted is never broadcast.
        if let Err(error) = self
            .kernel
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
        self.state.turn = None;
    }

    /// Abort the turn commit: journal the failed phase and the recovery
    /// requirement, then drop the turn frame. No further mandatory writes
    /// happen after a failure — they would build on a state that is already
    /// inconsistent.
    async fn commit_failed(&mut self, phase: TurnCommitPhase, error: AgentError) {
        let _ = self
            .kernel
            .emit_event(RuntimeEvent::TurnCommitFailed {
                phase: phase.as_str().into(),
                message: error.to_string(),
            })
            .await;
        let _ = self.kernel.emit_event(RuntimeEvent::RecoveryRequired).await;
        self.state.turn = None;
    }

    /// Close every tool frame the turn opened (from committed results and
    /// the in-flight op). Called before each model round — the request
    /// consumes the previous results — and on cancellation, so no tool
    /// scope outlives its execution frame. Already-closed scopes are no-ops.
    async fn close_tool_frames(&mut self) {
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
        for scope_id in scope_ids {
            match self.kernel.context_close_scope(scope_id).await {
                Ok(transitions) => {
                    // The close is an auditable result: publish the
                    // lifecycle transitions it produced (a tool frame's
                    // durable outcomes promoted out of the frame). An empty
                    // transition list is a no-op close — nothing to report.
                    if !transitions.is_empty() {
                        let _ = self
                            .kernel
                            .emit_event(RuntimeEvent::ToolScopeClosed {
                                scope_id,
                                transitions,
                            })
                            .await;
                    }
                }
                Err(error) => {
                    // A failed close must not be swallowed: surface it so
                    // the audit trail explains why the frame stayed open.
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::Error {
                            message: format!("closing tool scope {scope_id} failed: {error}"),
                        })
                        .await;
                }
            }
        }
    }

    /// Cancel the active turn: the cancellation decision is committed now
    /// (warning + TurnCompleted), the in-flight operation's late completion
    /// arrives stale and is dropped.
    async fn cancel_turn(&mut self) {
        self.close_tool_frames().await;
        if let Some(turn) = self.state.turn.as_mut() {
            if let Some(op) = turn.op.take() {
                op.cancel.cancel();
            }
            self.state.generation += 1;
            self.state.turn = None;
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::Warning {
                    message: "turn cancelled".into(),
                })
                .await;
            let _ = self.kernel.emit_event(RuntimeEvent::TurnCompleted).await;
        }
    }
}

/// Start the runtime: create the actor task and hand back a cloneable handle.
pub fn spawn_runtime(kernel: Arc<AgentKernel>) -> (RuntimeHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let (op_tx, op_rx) = mpsc::channel(16);
    let actor = RuntimeActor::new(kernel.clone());
    let handle = RuntimeHandle::new(tx, kernel.event_sender(), kernel.run_id());
    let task = tokio::spawn(actor.run(rx, op_tx, op_rx));
    (handle, task)
}

//! The runtime actor: owns the mutable runtime state and drives the turn
//! execution state machine. Model rounds and tool calls are *operations*:
//! they execute as spawned tasks and report an `OperationResult`; the actor
//! validates the result against the current generation and only then commits
//! it (context ingest/maintenance, turn-frame pushes, events). Stale results
//! are dropped — they never race into the new state.

use std::{collections::VecDeque, sync::Arc};

use agent_contracts::tokens::approx_tokens;
use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ContextHints, ContextIngress,
    ContextMaintenanceTrigger, ContextQuery, ContextRetention, Effect, EffectCommitError,
    ModelInput, ModelRequest, OperationId, OperationOutcome, OperationResult, RuntimeDirective,
    RuntimeEvent, ScopeId, ScopeKind, TaskId, ToolCall, ToolOutcome, ToolOutput,
    ToolSurfaceSnapshot, TurnFrame, TurnFrameStep, TurnId,
};
use agent_kernel::AgentKernel;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::budget::{DEFAULT_OUTPUT_RESERVE, ModelBudget, approx_layer_tokens};
use crate::checkpoint::{
    RUNTIME_CHECKPOINT_VERSION, RunMetadata, RuntimeCheckpoint, TaskManagerSnapshot,
};
use crate::command::{Reply, RuntimeCommand, RuntimeHandle};
use crate::prompt::PromptAssembler;
use crate::sink::LiveSink;
use crate::task::TaskManager;

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
    op: Option<InFlightOp>,
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
}

/// Mutable runtime state, owned exclusively by the actor loop. Callers never
/// touch it: everything goes through `RuntimeCommand`.
#[derive(Default)]
struct ActorState {
    /// Focus epoch. Bumped on every accepted turn, focus change and cancel;
    /// operations tagged with an older generation are stale.
    generation: u64,
    /// The task the runtime believes is current (updated by `set_focus`).
    task_id: Option<TaskId>,
    /// Long-lived task records; focus is attention inside the current task.
    tasks: TaskManager,
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
                let result = match self.ensure_idle() {
                    Ok(()) => {
                        // A task is the long-lived entity; focus is the
                        // attention inside it. `prepare_create` resumes a
                        // non-completed task with the same goal, so
                        // re-focusing returns to the same task id. The
                        // TaskManager transition is committed only after
                        // the engine's focus change succeeded, so the two
                        // can never diverge.
                        let (txn, task_id) = self.state.tasks.prepare_create(&goal);
                        match self.kernel.set_focus(task_id, goal).await {
                            Ok(()) => {
                                self.state.tasks.commit(txn);
                                self.state.task_id = Some(task_id);
                                self.state.generation += 1;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::ActivateTask { task_id, reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => match self.state.tasks.prepare_activate(task_id) {
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
                            match self.kernel.set_focus(task_id, goal).await {
                                Ok(()) => {
                                    self.state.tasks.commit(txn);
                                    self.state.task_id = Some(task_id);
                                    self.state.generation += 1;
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            }
                        }
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::SuspendTask { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => match self.state.tasks.prepare_suspend() {
                        None => Ok(()),
                        Some(txn) => match self.kernel.clear_focus().await {
                            Ok(()) => {
                                self.state.tasks.commit(txn);
                                self.state.task_id = None;
                                self.state.generation += 1;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        },
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::ListTasks { reply } => {
                let _ = reply.send(Ok(self.state.tasks.list()));
            }
            RuntimeCommand::Pin { content, reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => self.kernel.pin(content).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::CompleteTask { summary, reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => match self.state.tasks.prepare_complete() {
                        None => Err(AgentError::InvalidRequest(
                            "no active task to complete".into(),
                        )),
                        Some(txn) => {
                            // The engine resolves the completed task from the
                            // current focus; the TaskManager closes its record
                            // only after the engine's close succeeded.
                            match self.kernel.complete_current_task(summary).await {
                                Ok(()) => {
                                    self.state.tasks.commit(txn);
                                    self.state.task_id = None;
                                    self.state.generation += 1;
                                    Ok(())
                                }
                                Err(error) => Err(error),
                            }
                        }
                    },
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
                let result = match self.ensure_idle() {
                    Ok(()) => {
                        if checkpoint.version != RUNTIME_CHECKPOINT_VERSION {
                            Err(AgentError::InvalidRequest(format!(
                                "checkpoint version {} is not supported (expected {})",
                                checkpoint.version, RUNTIME_CHECKPOINT_VERSION
                            )))
                        } else {
                            // The engine's scopes were restored from the
                            // context payload; the task table and the
                            // current task come back in sync with them.
                            self.state.tasks.restore(checkpoint.tasks);
                            self.state.task_id = checkpoint.current_task_id;
                            self.state.generation += 1;
                            self.kernel.restore(checkpoint.context).await
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
        if self.state.turn.is_some() {
            Err(AgentError::InvalidRequest(
                "agent is busy: a turn is already running".into(),
            ))
        } else {
            Ok(())
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
            let (txn, task_id) = self.state.tasks.prepare_create(content.trim());
            if let Err(error) = self.kernel.set_focus(task_id, content.clone()).await {
                let _ = reply.send(Err(error));
                return;
            }
            self.state.tasks.commit(txn);
            self.state.task_id = Some(task_id);
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

        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.model_round += 1;
        let current_input = turn.turn_frame.user_message.clone();
        let turn_id = turn.turn_id;

        match self
            .kernel
            .context_maintain(ContextMaintenanceTrigger::BeforeModel)
            .await
        {
            Ok(report) => {
                let _ = self
                    .kernel
                    .emit_event(RuntimeEvent::ContextMaintained {
                        trigger: ContextMaintenanceTrigger::BeforeModel,
                        report,
                    })
                    .await;
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

        // Tool lifecycle safe point: age the tool catalog exactly once per
        // model round, then capture the round's tool surface snapshot. The
        // budget, the prompt and tool-call validation all use this one
        // snapshot, so the model sees — and the runtime validates against —
        // exactly the surface that was priced.
        self.kernel.tool_gc();
        let tool_surface = self.kernel.tool_snapshot();
        turn.tool_surface = Some(tool_surface.clone());

        // The engine only ever sees its own slice of the provider window:
        // the output reserve, system policy, turn frame and active tool
        // schemas are the runtime's share and are subtracted before the
        // engine budgets the working set.
        let capabilities = self.kernel.model_capabilities();
        let turn_frame_tokens = approx_layer_tokens(&turn.turn_frame.messages());
        let active_tools_tokens = approx_layer_tokens(&tool_surface.specs);
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
                hints: ContextHints::default(),
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
        let _ = self
            .kernel
            .emit_event(RuntimeEvent::ContextPrepared {
                diagnostics: materialized.diagnostics.clone(),
                selected: materialized.selected.clone(),
            })
            .await;
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let _ = self
            .kernel
            .emit_event(RuntimeEvent::ModelStarted {
                turn_id,
                operation_id,
                generation,
            })
            .await;

        // Runtime final guard: the engine priced the working-set content,
        // but the assembler's rendering overhead (section headers, per-item
        // frame labels) is the runtime's share. The assembled request must
        // fit the *input* budget — the window minus the output reserve —
        // because the answer must always have room. Trim the context frame
        // until it fits; if the fixed layers alone (system + turn + tools)
        // still overshoot, unload the largest optional tools; a request
        // that still does not fit is a hard error, never a silently
        // over-budget send.
        let max_input_budget = provider_window.saturating_sub(output_reserve);
        let mut materialized = materialized;
        let mut tool_surface = tool_surface;
        let mut input =
            self.assembler
                .assemble(&materialized, &turn.turn_frame, tool_surface.specs.clone());
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
            input = self.assembler.assemble(
                &materialized,
                &turn.turn_frame,
                tool_surface.specs.clone(),
            );
        }

        // The context frame is empty but the fixed layers still overshoot:
        // unload the largest optional tools one by one (core tools refuse
        // and are skipped) so the request goes out with the leanest
        // surface, then re-snapshot so the round validates against exactly
        // the surface that was priced.
        let mut tried: std::collections::HashSet<String> = std::collections::HashSet::new();
        while assembled_total(&input) > max_input_budget {
            let Some((name, _)) = tool_surface
                .specs
                .iter()
                .filter(|spec| !tried.contains(&spec.name))
                .map(|spec| (spec.name.clone(), approx_layer_tokens(spec)))
                .max_by_key(|(_, tokens)| *tokens)
            else {
                break;
            };
            tried.insert(name.clone());
            if self.kernel.tool_unload(&name).is_ok() {
                tool_surface = self.kernel.tool_snapshot();
                turn.tool_surface = Some(tool_surface.clone());
                input = self.assembler.assemble(
                    &materialized,
                    &turn.turn_frame,
                    tool_surface.specs.clone(),
                );
            }
        }
        if assembled_total(&input) > max_input_budget {
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::Error {
                    message: format!(
                        "model input exceeds the provider window even with the context frame emptied and optional tools unloaded ({} > {} input tokens); refusing to send",
                        assembled_total(&input),
                        max_input_budget
                    ),
                })
                .await;
            self.state.turn = None;
            return;
        }

        let cancel = CancellationToken::new();
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            scope_id: None,
            cancel: cancel.clone(),
        });

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
                },
                Err(AgentError::Cancelled) => OperationOutcome::Cancelled,
                Err(error) => OperationOutcome::Failed {
                    message: error.to_string(),
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
            let (operation, effect, directive) = match outcome {
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
                ),
                ToolOutcome::RuntimeDirective { output, directive } => (
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
                ),
                // The tool asked the runtime to resolve a read-only engine
                // query: the kernel (the ContextEngine owner) answers and
                // the placeholder output becomes the final one. No effect,
                // no directive — search/inspect/fetch are pure reads.
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
                    )
                }
            };
            let _ = op_tx
                .send(OperationCompletion {
                    operation,
                    kind: OpKind::Tool,
                    effect,
                    directive,
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
        if let Some(turn) = self.state.turn.as_mut() {
            turn.op = None;
        }
        match completion.operation.outcome {
            OperationOutcome::ModelOutput {
                content,
                tool_calls,
            } => {
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
                // Execute the tool's runtime directive now, as part of the
                // operation commit — not at turn end — so a context control
                // request takes effect before the next model round.
                if let Some(directive) = completion.directive {
                    self.execute_directive(directive).await;
                }
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame
                        .push_tool_result(output.clone(), op_scope_id);
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
                if let Ok(report) = self.kernel.context_gc().await {
                    let _ = self
                        .kernel
                        .emit_event(RuntimeEvent::ContextGc { report })
                        .await;
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
    async fn finalize_turn(&mut self, content: String) {
        let mut ingested = false;
        if let Some(turn) = self.state.turn.as_mut() {
            for step in &turn.turn_frame.steps {
                if let TurnFrameStep::ToolResult { output, scope_id } = step
                    && self
                        .kernel
                        .context_ingest(ContextIngress::ToolObservation {
                            output: output.clone(),
                            scope_id: *scope_id,
                        })
                        .await
                        .is_ok()
                {
                    ingested = true;
                }
            }
        }
        if ingested
            && let Ok(report) = self
                .kernel
                .context_maintain(ContextMaintenanceTrigger::AfterTool)
                .await
        {
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterTool,
                    report,
                })
                .await;
        }
        if let Err(error) = self
            .kernel
            .context_ingest(ContextIngress::AssistantMessage {
                content: content.clone(),
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
        let _ = self
            .kernel
            .emit_event(RuntimeEvent::AssistantMessage { content })
            .await;
        if let Ok(report) = self
            .kernel
            .context_maintain(ContextMaintenanceTrigger::AfterModel)
            .await
        {
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report,
                })
                .await;
        }
        // Turn boundary: the full GC pass compacts what the per-event
        // residency machine demoted. Eviction is reversible, and the report
        // explains every eviction and reactivation.
        if let Ok(report) = self.kernel.context_gc().await {
            let _ = self
                .kernel
                .emit_event(RuntimeEvent::ContextGc { report })
                .await;
        }
        let _ = self.kernel.emit_event(RuntimeEvent::TurnCompleted).await;
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
            let _ = self.kernel.context_close_scope(scope_id).await;
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

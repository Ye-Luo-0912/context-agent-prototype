//! The runtime actor: owns the mutable runtime state and drives the turn
//! execution state machine. Model rounds and tool calls are *operations*:
//! they execute as spawned tasks and report an `OperationResult`; the actor
//! validates the result against the current generation and only then commits
//! it (context ingest/maintenance, turn-frame pushes, events). Stale results
//! are dropped — they never race into the new state.

use std::{collections::VecDeque, sync::Arc};

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ContextHints, ContextIngress,
    ContextMaintenanceTrigger, ContextQuery, ModelRequest, OperationId, OperationOutcome,
    OperationResult, RuntimeEvent, ScopeId, TaskId, ToolCall, TurnFrame, TurnFrameStep, TurnId,
};
use agent_kernel::AgentKernel;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::command::{Reply, RuntimeCommand, RuntimeHandle};
use crate::prompt::PromptAssembler;
use crate::sink::LiveSink;

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
    cancel: CancellationToken,
}

/// The runtime's view of the active turn: the execution stack (TurnFrame),
/// how many model rounds ran, tool calls still waiting to run, and the
/// in-flight operation.
struct ActiveTurn {
    turn_id: TurnId,
    turn_frame: TurnFrame,
    model_round: usize,
    pending_tools: VecDeque<ToolCall>,
    op: Option<InFlightOp>,
}

/// What a spawned operation reports when it finishes.
pub(crate) struct OperationCompletion {
    operation: OperationResult,
    kind: OpKind,
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
                            self.cancel_turn().await;
                            let _ = self.kernel.stop().await;
                            let _ = reply.send(());
                            return;
                        }
                        Some(command) => self.process(command, &op_tx).await,
                        None => return,
                    }
                }
                completed = op_rx.recv() => {
                    match completed {
                        Some(completion) => self.on_operation_completed(completion, &op_tx).await,
                        None => return,
                    }
                }
            }
        }
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
                    Ok(()) => match self.kernel.set_focus(goal).await {
                        Ok(task_id) => {
                            self.state.task_id = Some(task_id);
                            self.state.generation += 1;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
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
                    Ok(()) => self.kernel.complete_current_task(summary).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            RuntimeCommand::Checkpoint { reply } => {
                let result = match self.ensure_idle() {
                    Ok(()) => self.kernel.checkpoint().await,
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

    /// Prepare + spawn one model round: maintenance, snapshot, model input
    /// assembly, then the model call as an operation.
    async fn spawn_model_operation(&mut self, op_tx: &mpsc::Sender<OperationCompletion>) {
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

        // The system prompt is the runtime's own overhead; the context
        // engine budgets for the working set with the rest.
        let budget = self
            .kernel
            .context_budget_tokens()
            .saturating_sub(self.assembler.system_prompt_tokens());
        let materialized = match self
            .kernel
            .context_materialize(ContextQuery {
                current_input: current_input.clone(),
                budget_tokens: budget,
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
        let _ = self.kernel.emit_event(RuntimeEvent::ModelStarted).await;

        let input =
            self.assembler
                .assemble(&materialized, &turn.turn_frame, self.kernel.tool_specs());
        let cancel = CancellationToken::new();
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        turn.op = Some(InFlightOp {
            operation_id,
            turn_id,
            generation,
            cancel: cancel.clone(),
        });

        let kernel = self.kernel.clone();
        let sink = LiveSink::new(kernel.event_sender(), kernel.seq(), kernel.run_id());
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
                })
                .await;
        });
    }

    /// Prepare + spawn one tool call: emit ToolStarted, then run approval +
    /// dispatch as an operation.
    async fn spawn_tool_operation(
        &mut self,
        call: ToolCall,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        let turn_id = turn.turn_id;
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
            cancel: cancel.clone(),
        });

        let kernel = self.kernel.clone();
        let op_tx = op_tx.clone();
        let run_id = kernel.run_id();
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        tokio::spawn(async move {
            let output = kernel.execute_tool(call, cancel).await;
            let _ = op_tx
                .send(OperationCompletion {
                    operation: OperationResult {
                        run_id,
                        turn_id,
                        task_id,
                        scope_id,
                        operation_id,
                        generation,
                        outcome: OperationOutcome::ToolOutput(output),
                    },
                    kind: OpKind::Tool,
                })
                .await;
        });
    }

    /// Verify a finished operation still belongs to the current turn and
    /// generation. Stale completions (cancelled or superseded) are dropped
    /// and surfaced as a warning; live ones are committed.
    async fn on_operation_completed(
        &mut self,
        completion: OperationCompletion,
        op_tx: &mpsc::Sender<OperationCompletion>,
    ) {
        if self.is_stale(&completion) {
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
                if let Some(turn) = self.state.turn.as_mut() {
                    turn.turn_frame.push_tool_result(output.clone());
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

    /// When the model stops calling tools, the turn's tool observations
    /// become the long-term record, then the final assistant message.
    async fn finalize_turn(&mut self, content: String) {
        let mut ingested = false;
        if let Some(turn) = self.state.turn.as_mut() {
            for step in &turn.turn_frame.steps {
                if let TurnFrameStep::ToolResult { output } = step
                    && self
                        .kernel
                        .context_ingest(ContextIngress::ToolObservation {
                            output: output.clone(),
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
        let _ = self.kernel.emit_event(RuntimeEvent::TurnCompleted).await;
        self.state.turn = None;
    }

    /// Cancel the active turn: the cancellation decision is committed now
    /// (warning + TurnCompleted), the in-flight operation's late completion
    /// arrives stale and is dropped.
    async fn cancel_turn(&mut self) {
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

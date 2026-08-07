//! The runtime actor: owns the mutable runtime state and serializes every
//! command. Long-running work (a turn) runs as a spawned operation that
//! reports back through an `OperationResult`; results whose generation moved
//! on are dropped instead of racing into the new state.

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, OperationId, OperationOutcome, OperationResult, ScopeId, TaskId,
    TurnId,
};
use agent_kernel::AgentKernel;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::command::{Reply, RuntimeCommand, RuntimeHandle};

/// The actor's view of a running turn (the full identity lives in the
/// operation's `OperationResult`; the actor only needs the turn id to
/// recognize a stale completion).
struct ActiveTurn {
    turn_id: TurnId,
}

/// What a spawned turn reports when it finishes.
pub(crate) struct TurnCompletion {
    operation: OperationResult,
}

/// Mutable runtime state, owned exclusively by the actor loop. Callers never
/// touch it: everything goes through `RuntimeCommand`.
#[derive(Default)]
struct ActorState {
    /// Focus epoch. Bumped on every accepted turn, focus change and cancel;
    /// operations tagged with an older generation are stale.
    generation: u64,
    turn_id: Option<TurnId>,
    /// The task the runtime believes is current (updated by `set_focus`).
    task_id: Option<TaskId>,
    /// The runtime's view of the current scope (filled once the context
    /// engine exposes its scope tree through the contract).
    scope_id: Option<ScopeId>,
    active_turn: Option<ActiveTurn>,
}

pub struct RuntimeActor {
    kernel: Arc<AgentKernel>,
    state: ActorState,
}

impl RuntimeActor {
    pub fn new(kernel: Arc<AgentKernel>) -> Self {
        Self {
            kernel,
            state: ActorState::default(),
        }
    }

    /// Run the actor loop until `Stop` or all handles are dropped. Commands
    /// and turn completions are handled concurrently so cancellation can
    /// always be processed while a turn is running.
    pub(crate) async fn run(
        mut self,
        mut rx: mpsc::Receiver<RuntimeCommand>,
        turn_tx: mpsc::Sender<TurnCompletion>,
        mut turn_rx: mpsc::Receiver<TurnCompletion>,
    ) {
        loop {
            tokio::select! {
                command = rx.recv() => {
                    match command {
                        Some(RuntimeCommand::Stop { reply }) => {
                            if self.state.active_turn.take().is_some() {
                                self.state.generation += 1;
                                self.state.turn_id = None;
                                self.kernel.cancel_current_turn().await;
                            }
                            let _ = self.kernel.stop().await;
                            let _ = reply.send(());
                            return;
                        }
                        Some(command) => self.process(command, &turn_tx).await,
                        None => return,
                    }
                }
                completed = turn_rx.recv() => {
                    match completed {
                        Some(completion) => self.on_turn_completed(completion).await,
                        None => return,
                    }
                }
            }
        }
    }

    async fn process(&mut self, command: RuntimeCommand, turn_tx: &mpsc::Sender<TurnCompletion>) {
        match command {
            RuntimeCommand::Start { reply } => {
                let _ = reply.send(self.kernel.start().await);
            }
            RuntimeCommand::UserMessage { content, reply } => {
                self.start_turn(content, reply, turn_tx).await;
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
                if self.state.active_turn.take().is_some() {
                    self.state.generation += 1;
                    self.state.turn_id = None;
                    self.kernel.cancel_current_turn().await;
                }
                let _ = reply.send(());
            }
            RuntimeCommand::Stop { .. } => unreachable!("Stop is handled in the run loop"),
        }
    }

    /// A turn is accepted only when the runtime is idle. Serializing every
    /// mutation removes the structural race where focus/pin/task commands
    /// interleaved with an in-flight turn.
    fn ensure_idle(&self) -> AgentResult<()> {
        if self.state.active_turn.is_some() {
            Err(AgentError::InvalidRequest(
                "agent is busy: a turn is already running".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Accept a user message by spawning the turn as an operation. The actor
    /// replies immediately; the turn reports back through the completion
    /// channel when it ends.
    async fn start_turn(
        &mut self,
        content: String,
        reply: Reply<AgentResult<()>>,
        turn_tx: &mpsc::Sender<TurnCompletion>,
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
        let operation_id = OperationId::new();
        let generation = self.state.generation;
        let task_id = self.state.task_id;
        let scope_id = self.state.scope_id;
        let run_id = self.kernel.run_id();
        self.state.turn_id = Some(turn_id);
        self.state.active_turn = Some(ActiveTurn { turn_id });
        let kernel = self.kernel.clone();
        let turn_tx = turn_tx.clone();
        tokio::spawn(async move {
            let result = kernel.handle_user_message(content).await;
            let outcome = match &result {
                Ok(()) => OperationOutcome::Completed,
                Err(AgentError::Cancelled) => OperationOutcome::Cancelled,
                Err(error) => OperationOutcome::Failed {
                    message: error.to_string(),
                },
            };
            let operation = OperationResult {
                run_id,
                turn_id,
                task_id,
                scope_id,
                operation_id,
                generation,
                outcome,
            };
            let _ = turn_tx.send(TurnCompletion { operation }).await;
        });
        let _ = reply.send(Ok(()));
    }

    /// Verify a finished turn still belongs to the current focus. A stale
    /// completion (cancelled or superseded) is dropped and surfaced as a
    /// warning; a live one just clears the busy marker — the kernel already
    /// committed its own terminal events.
    async fn on_turn_completed(&mut self, completion: TurnCompletion) {
        let current_turn = self.state.active_turn.as_ref().map(|active| active.turn_id);
        if completion
            .operation
            .is_stale(current_turn, self.state.generation)
        {
            let message = format!(
                "stale turn result dropped (turn {}, generation {})",
                completion.operation.turn_id, completion.operation.generation
            );
            if let Err(error) = self.kernel.emit_warning(message).await {
                tracing::warn!(%error, "failed to emit stale-result warning");
            }
            return;
        }
        self.state.active_turn = None;
        self.state.turn_id = None;
    }
}

/// Start the runtime: create the actor task and hand back a cloneable handle.
pub fn spawn_runtime(kernel: Arc<AgentKernel>) -> (RuntimeHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(64);
    let (turn_tx, turn_rx) = mpsc::channel(16);
    let actor = RuntimeActor::new(kernel.clone());
    let handle = RuntimeHandle::new(tx, kernel.event_sender(), kernel.run_id());
    let task = tokio::spawn(actor.run(rx, turn_tx, turn_rx));
    (handle, task)
}

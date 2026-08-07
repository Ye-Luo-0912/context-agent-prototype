//! Runtime commands and the handle callers use to drive the actor.

use agent_contracts::{AgentError, AgentResult, ContextItemSummary, RunId, RuntimeEventEnvelope};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Reply channel back to the caller of a command.
pub type Reply<T> = oneshot::Sender<T>;

/// Everything a caller can ask the runtime to do. Commands are serialized by
/// the actor: no two runtime mutations run concurrently, which removes the
/// structural race where focus/pin/task commands interleaved with a turn.
#[derive(Debug)]
pub enum RuntimeCommand {
    Start {
        reply: Reply<AgentResult<()>>,
    },
    UserMessage {
        content: String,
        reply: Reply<AgentResult<()>>,
    },
    SetFocus {
        goal: String,
        reply: Reply<AgentResult<()>>,
    },
    Pin {
        content: String,
        reply: Reply<AgentResult<()>>,
    },
    CompleteTask {
        summary: String,
        reply: Reply<AgentResult<()>>,
    },
    Checkpoint {
        reply: Reply<AgentResult<serde_json::Value>>,
    },
    EmitDiagnostics {
        reply: Reply<AgentResult<()>>,
    },
    InspectContext {
        limit: usize,
        reply: Reply<AgentResult<Vec<ContextItemSummary>>>,
    },
    CancelTurn {
        reply: Reply<()>,
    },
    Stop {
        reply: Reply<()>,
    },
}

/// A cloneable front door to the runtime actor. Every call sends one command
/// and awaits the actor's reply, so callers never mutate runtime state
/// directly.
#[derive(Clone)]
pub struct RuntimeHandle {
    tx: mpsc::Sender<RuntimeCommand>,
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    run_id: RunId,
}

impl RuntimeHandle {
    pub(crate) fn new(
        tx: mpsc::Sender<RuntimeCommand>,
        event_tx: broadcast::Sender<RuntimeEventEnvelope>,
        run_id: RunId,
    ) -> Self {
        Self {
            tx,
            event_tx,
            run_id,
        }
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Subscribe to the runtime event stream (kernel events plus actor-level
    /// warnings). New subscriptions see events from this point on.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.event_tx.subscribe()
    }

    /// Start the runtime (emits `RunStarted`). Subscribe first to see it.
    pub async fn start(&self) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::Start { reply }).await
    }

    /// Run one user turn. Returns once the turn is accepted (it runs in the
    /// background); errors when the runtime is busy with another turn.
    pub async fn user_message(&self, content: String) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::UserMessage { content, reply })
            .await
    }

    pub async fn set_focus(&self, goal: String) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::SetFocus { goal, reply })
            .await
    }

    pub async fn pin(&self, content: String) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::Pin { content, reply })
            .await
    }

    pub async fn complete_current_task(&self, summary: String) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::CompleteTask { summary, reply })
            .await
    }

    pub async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        self.call(|reply| RuntimeCommand::Checkpoint { reply })
            .await
    }

    pub async fn emit_diagnostics(&self) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::EmitDiagnostics { reply })
            .await
    }

    pub async fn inspect_context(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.call(|reply| RuntimeCommand::InspectContext { limit, reply })
            .await
    }

    /// Cancel the in-flight turn (if any). The actor immediately considers
    /// the turn superseded, so its late completion is dropped as stale.
    pub async fn cancel_turn(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(RuntimeCommand::CancelTurn { reply: tx })
            .await
            .is_err()
        {
            return;
        }
        let _ = rx.await;
    }

    /// Stop the runtime: cancel any turn, stop the kernel and end the actor.
    pub async fn stop(&self) -> AgentResult<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RuntimeCommand::Stop { reply: tx })
            .await
            .map_err(|_| AgentError::Internal("runtime actor already stopped".into()))?;
        rx.await
            .map_err(|_| AgentError::Internal("runtime actor stopped without reply".into()))
    }

    /// Send a command whose reply carries a `Result` and return the inner
    /// result (channel failures become `AgentError::Internal`).
    async fn call<T>(
        &self,
        make: impl FnOnce(Reply<AgentResult<T>>) -> RuntimeCommand,
    ) -> AgentResult<T> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| AgentError::Internal("runtime actor stopped".into()))?;
        rx.await
            .map_err(|_| AgentError::Internal("runtime actor dropped the reply".into()))?
    }
}

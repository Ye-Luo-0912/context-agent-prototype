//! Runtime commands and the handle callers use to drive the actor.

use agent_contracts::{
    AgentError, AgentResult, ContextItemSummary, OperationId, OperationQueryResult, RunId,
    RuntimeEventEnvelope, TaskId, ToolOperationIdentity, ToolSurfaceRequirement, TurnCancelAck,
};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::checkpoint::RuntimeCheckpoint;
use crate::task::{AnchorPatch, TaskAnchor, TaskInfo};

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
    /// 用户对话。Actor 收成 `RuntimeInputEnvelope`（Dialogue / UserSteering）。
    /// 周转中最多排队一条；`/cancel` `/focus` `/done` 不走这条命令。
    UserMessage {
        content: String,
        reply: Reply<AgentResult<()>>,
    },
    /// 显式改焦点。直达命令，不经 UserMessage 解释。
    SetFocus {
        goal: String,
        reply: Reply<AgentResult<()>>,
    },
    /// Activate an existing task by id (resuming its scopes in the engine).
    ActivateTask {
        task_id: TaskId,
        reply: Reply<AgentResult<()>>,
    },
    /// Suspend the active task without completing it (focus cleared).
    SuspendTask {
        reply: Reply<AgentResult<()>>,
    },
    /// List the tasks the runtime knows (for the UI's `/tasks`).
    ListTasks {
        reply: Reply<AgentResult<Vec<TaskInfo>>>,
    },
    /// Read-only projection of the active task's anchor (plan progress,
    /// open loops, next action). Reading never mutates state and never
    /// writes a checkpoint; `None` when no task is active.
    TaskPlanView {
        reply: Reply<AgentResult<Option<agent_contracts::TaskAnchorView>>>,
    },
    /// Atomically replace one task's complete tool-demand set when
    /// `base_revision` still matches.
    ReplaceTaskToolRequirements {
        task_id: TaskId,
        base_revision: u64,
        entries: Vec<ToolSurfaceRequirement>,
        reply: Reply<AgentResult<u64>>,
    },
    /// Atomically replace one task's whole anchor (bounded, versioned) when
    /// `base_revision` still matches.
    UpdateTaskAnchor {
        task_id: TaskId,
        base_revision: u64,
        anchor: TaskAnchor,
        reply: Reply<AgentResult<u64>>,
    },
    /// Atomically apply a bounded, field-level patch to one task's anchor
    /// when `base_revision` still matches. The patch is classified before
    /// it reaches the task table: evolution fields apply autonomously,
    /// goal/constraint fields must clear the approval gate first.
    PatchTaskAnchor {
        task_id: TaskId,
        base_revision: u64,
        patch: AnchorPatch,
        reply: Reply<AgentResult<u64>>,
    },
    Pin {
        content: String,
        reply: Reply<AgentResult<()>>,
    },
    /// `/done`：直达完成命令，不经 UserMessage。
    CompleteTask {
        summary: String,
        reply: Reply<AgentResult<()>>,
    },
    Checkpoint {
        reply: Reply<AgentResult<RuntimeCheckpoint>>,
    },
    /// Continue the active task's current directive in a fresh turn after
    /// a stop/restore. No new user instruction is minted and the stored
    /// directive identity does not change.
    ContinueActiveTask {
        reply: Reply<AgentResult<()>>,
    },
    /// Prepare a full restore: transactionally install context + task
    /// authority, then leave the actor fenced until the host has applied
    /// the capability plane and sends `FinalizeRestore`.
    PrepareRestore {
        checkpoint: RuntimeCheckpoint,
        reply: Reply<AgentResult<u64>>,
    },
    /// Publish the durable restore-commit record with the host's actual
    /// capability-application result. Only a successful barrier clears the
    /// recovery fence left by `PrepareRestore`.
    FinalizeRestore {
        restore_id: u64,
        capabilities_applied: bool,
        reply: Reply<AgentResult<()>>,
    },
    EmitDiagnostics {
        reply: Reply<AgentResult<()>>,
    },
    InspectContext {
        limit: usize,
        reply: Reply<AgentResult<Vec<ContextItemSummary>>>,
    },
    /// Read Core's bounded authority truth for one tool operation. This is
    /// diagnostic/control-plane state, not a request to redispatch work.
    QueryOperation {
        operation_id: OperationId,
        reply: Reply<AgentResult<OperationQueryResult>>,
    },
    /// Cancel only the current in-flight tool operation when every identity
    /// field matches. Historical, model, and partially matching operations
    /// are deliberately outside this V1 control surface.
    CancelOperation {
        identity: ToolOperationIdentity,
        reply: Reply<AgentResult<OperationQueryResult>>,
    },
    /// `/cancel`：直达取消，不经 UserMessage 信封解释。
    CancelTurn {
        reply: Reply<AgentResult<TurnCancelAck>>,
    },
    Stop {
        reply: Reply<AgentResult<()>>,
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

    /// Activate an existing task, resuming its scopes in the context engine.
    pub async fn activate_task(&self, task_id: TaskId) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::ActivateTask { task_id, reply })
            .await
    }

    /// Suspend the active task without completing it.
    pub async fn suspend_task(&self) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::SuspendTask { reply })
            .await
    }

    /// Snapshot of the tasks the runtime knows.
    pub async fn list_tasks(&self) -> AgentResult<Vec<TaskInfo>> {
        self.call(|reply| RuntimeCommand::ListTasks { reply }).await
    }

    /// Bounded read-only view of the active task's anchor: the checklist
    /// (`plan_progress`), open loops and next action the model maintains
    /// through `task.manage`. `None` when no task is active. Reading does
    /// not write state and does not create a checkpoint.
    pub async fn task_plan_view(&self) -> AgentResult<Option<agent_contracts::TaskAnchorView>> {
        self.call(|reply| RuntimeCommand::TaskPlanView { reply })
            .await
    }

    /// Replace a task's bounded tool-demand set through whole-set CAS and
    /// return the resulting revision. An equivalent set is idempotent and
    /// returns the existing revision.
    pub async fn replace_task_tool_requirements(
        &self,
        task_id: TaskId,
        base_revision: u64,
        entries: Vec<ToolSurfaceRequirement>,
    ) -> AgentResult<u64> {
        self.call(|reply| RuntimeCommand::ReplaceTaskToolRequirements {
            task_id,
            base_revision,
            entries,
            reply,
        })
        .await
    }

    /// Replace a task's whole anchor through whole-set CAS and return the
    /// resulting revision. An equivalent anchor is idempotent and returns
    /// the existing revision.
    pub async fn update_task_anchor(
        &self,
        task_id: TaskId,
        base_revision: u64,
        anchor: TaskAnchor,
    ) -> AgentResult<u64> {
        self.call(|reply| RuntimeCommand::UpdateTaskAnchor {
            task_id,
            base_revision,
            anchor,
            reply,
        })
        .await
    }

    /// Apply a bounded, field-level patch to a task's anchor through CAS
    /// and return the resulting revision. Evolution fields apply
    /// autonomously; goal/constraint fields first clear the approval gate
    /// (a denied boundary patch errors without touching the anchor). An
    /// equivalent patch is idempotent and returns the existing revision.
    pub async fn patch_task_anchor(
        &self,
        task_id: TaskId,
        base_revision: u64,
        patch: AnchorPatch,
    ) -> AgentResult<u64> {
        self.call(|reply| RuntimeCommand::PatchTaskAnchor {
            task_id,
            base_revision,
            patch,
            reply,
        })
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

    /// Capture the actor-owned checkpoint plane for `RuntimeInstance`.
    /// Kept crate-private because this value deliberately omits host-owned
    /// capability state and must never be persisted as a complete runtime
    /// checkpoint by an external caller.
    pub(crate) async fn checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        self.call(|reply| RuntimeCommand::Checkpoint { reply })
            .await
    }

    /// Continue the active task's stored current directive in a fresh
    /// turn. Public: stop/restore twins are a host-driven flow.
    pub async fn continue_active_task(&self) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::ContinueActiveTask { reply })
            .await
    }

    /// First half of `RuntimeInstance::restore`. Kept crate-private so a
    /// caller cannot prepare actor state and forget to finalize it.
    pub(crate) async fn prepare_restore(&self, checkpoint: RuntimeCheckpoint) -> AgentResult<u64> {
        self.call(|reply| RuntimeCommand::PrepareRestore { checkpoint, reply })
            .await
    }

    /// Second half of `RuntimeInstance::restore`: durably commit the
    /// prepared state after the host capability plane has been applied.
    pub(crate) async fn finalize_restore(
        &self,
        restore_id: u64,
        capabilities_applied: bool,
    ) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::FinalizeRestore {
            restore_id,
            capabilities_applied,
            reply,
        })
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

    /// Read Core's exact retained truth for one tool operation. The three
    /// results stay distinct: `ExpiredOrPossiblySeen` must never be treated
    /// as `NotFound` or permission to execute the operation again.
    ///
    /// This trusted in-process seam performs no external caller
    /// authorization. A Platform router must enforce run visibility before
    /// forwarding an operation id.
    pub async fn query_operation(
        &self,
        operation_id: OperationId,
    ) -> AgentResult<OperationQueryResult> {
        self.call(|reply| RuntimeCommand::QueryOperation {
            operation_id,
            reply,
        })
        .await
    }

    /// Cancel the current in-flight tool operation through the owning turn.
    /// The complete identity must match Runtime's active operation exactly;
    /// a newly won cancellation is returned only after Core records
    /// `CancelledBeforeCommit` and the distinct `TurnCancelled` durability
    /// barrier succeeds. If Core had already reached a terminal or
    /// `CommitStarted` state, `Ok` instead carries that pre-existing truth
    /// without relabelling it as a successful cancellation.
    ///
    /// This trusted in-process seam performs no external authorization. A
    /// Platform router must verify run visibility and control permission,
    /// query by the envelope's work identity, and canonicalize the complete
    /// identity from Core's snapshot before calling it. Query can recover
    /// Core cancellation truth after a lost reply, but it does not prove the
    /// actor's distinct durable `TurnCancelled` acknowledgement; without a
    /// persisted actor ACK marker, cancellation must not be reported as a
    /// retry success. This method is intentionally not a general historical-
    /// operation kill or blind-retry API.
    /// `CancelledBeforeCommit` only proves the Core-mediated commit did not
    /// start; it cannot undo mutations a non-transactional child process
    /// already performed before observing cancellation.
    pub async fn cancel_operation(
        &self,
        identity: ToolOperationIdentity,
    ) -> AgentResult<OperationQueryResult> {
        self.call(|reply| RuntimeCommand::CancelOperation { identity, reply })
            .await
    }

    /// Cancel the in-flight turn (if any). The actor immediately considers
    /// the turn superseded, so its late completion is dropped as stale.
    pub async fn cancel_turn(&self) -> AgentResult<TurnCancelAck> {
        self.call(|reply| RuntimeCommand::CancelTurn { reply })
            .await
    }

    /// Stop the runtime: cancel any turn, stop the kernel (flush the journal,
    /// emit `RunCompleted`) and end the actor. The kernel stop result — and
    /// with it any flush failure — is returned instead of swallowed.
    pub async fn stop(&self) -> AgentResult<()> {
        self.call(|reply| RuntimeCommand::Stop { reply }).await
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

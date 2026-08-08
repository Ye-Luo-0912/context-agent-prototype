use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, ContextDiagnostics, ContextGcReport, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextSelection, OperationId, RunId, TaskId, ToolCall, ToolOutput,
    TurnId,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEventEnvelope {
    pub run_id: RunId,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RunStarted,
    UserMessageAccepted {
        content: String,
    },
    FocusChanged {
        task_id: TaskId,
        goal: String,
    },
    FocusCleared,
    Pinned {
        content: String,
    },
    ContextPrepared {
        diagnostics: ContextDiagnostics,
        #[serde(default)]
        selected: Vec<ContextSelection>,
    },
    ContextMaintained {
        #[serde(default)]
        trigger: ContextMaintenanceTrigger,
        report: ContextMaintenanceReport,
    },
    /// A full GC pass ran: roots were marked, unmarked items were evicted to
    /// the reversible buffer and/or reactivated. The report explains every
    /// eviction and reactivation.
    ContextGc {
        report: ContextGcReport,
    },
    /// A model round started. Carries the operation identity so live
    /// consumers (the UI's run-state aggregator) can fence streamed deltas:
    /// a delta whose turn/operation/generation no longer matches the
    /// current round belongs to a superseded turn and must be dropped.
    ModelStarted {
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
    },
    /// Live streamed text delta. Never journaled (the final `AssistantMessage`
    /// carries the complete content); only forwarded to live subscribers.
    /// The operation identity is the fence: a late delta from a cancelled
    /// turn must not be rendered into the next turn's transcript.
    ModelDelta {
        turn_id: TurnId,
        operation_id: OperationId,
        generation: u64,
        delta: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        output: ToolOutput,
    },
    Diagnostics {
        diagnostics: ContextDiagnostics,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    TaskCompleted {
        summary: String,
    },
    TurnCompleted,
    RunCompleted,
}

#[async_trait]
pub trait EventJournal: Send + Sync {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()>;

    async fn flush(&self) -> AgentResult<()> {
        Ok(())
    }
}

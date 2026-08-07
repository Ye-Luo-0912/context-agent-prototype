use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, ContextDiagnostics, ContextGcReport, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextSelection, RunId, ToolCall, ToolOutput,
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
        goal: String,
    },
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
    ModelStarted,
    /// Live streamed text delta. Never journaled (the final `AssistantMessage`
    /// carries the complete content); only forwarded to live subscribers.
    ModelDelta {
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

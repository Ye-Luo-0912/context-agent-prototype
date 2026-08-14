//! Runtime contracts for the actor-based runtime: operation identity and the
//! typed capability vocabulary of the module host.

use serde::{Deserialize, Serialize};

use crate::{
    AgentResult, ApprovalGate, ContextEngine, EventJournal, ModelTransport, ModelUsage,
    OperationId, RunId, ScopeId, TaskId, ToolCall, ToolDispatcher, ToolOutput, TurnId,
};

/// Why the runtime terminated a turn without committing a model result.
/// This is a closed, bounded vocabulary so recovery and UI consumers never
/// infer cancellation semantics from a warning string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancellationReason {
    /// An operator/client explicitly requested cancellation.
    Requested,
    /// The active model/tool operation itself reported cancellation.
    OperationCancelled,
    /// Ordered runtime shutdown cancelled the still-active turn.
    Shutdown,
}

/// Durable acknowledgement returned by `RuntimeHandle::cancel_turn`.
/// `Cancelled` means the turn was fenced and its `TurnCancelled` event passed
/// the journal barrier. A barrier failure is returned as `RecoveryRequired`
/// instead of manufacturing this acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnCancelAck {
    NoActiveTurn,
    Cancelled {
        turn_id: TurnId,
        task_id: Option<TaskId>,
        operation_id: Option<OperationId>,
        cancelled_generation: u64,
        effective_generation: u64,
    },
}

/// What a long-running operation produced. The actor compares the identity
/// (run/turn/operation/generation) with the current state and drops results
/// that belong to a superseded turn instead of letting them race in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OperationOutcome {
    /// A model round finished with content and optional tool calls.
    ModelOutput {
        content: String,
        tool_calls: Vec<ToolCall>,
        /// Provider-reported usage for this round, so the runtime can emit
        /// `RuntimeEvent::ModelUsed` at commit time without re-parsing the
        /// output.
        usage: ModelUsage,
    },
    /// A tool execution produced its bounded output.
    ToolOutput(ToolOutput),
    /// The operation finished without a value to carry (a whole turn, a
    /// checkpoint, a focus change).
    Completed,
    Failed {
        message: String,
    },
    Cancelled,
}

/// Identity and outcome of one operation. Every long-running piece of work
/// (a model round, a tool call, a turn) reports back through this type so the
/// runtime can verify the result still belongs to the current focus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationResult {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub task_id: Option<TaskId>,
    pub scope_id: Option<ScopeId>,
    pub operation_id: OperationId,
    pub generation: u64,
    pub outcome: OperationOutcome,
}

impl OperationResult {
    /// True when the result belongs to a superseded turn: the generation
    /// moved on (cancel, focus change) or the turn id no longer matches.
    pub fn is_stale(&self, current_turn: Option<TurnId>, current_generation: u64) -> bool {
        current_generation != self.generation || current_turn != Some(self.turn_id)
    }
}

/// Every typed capability is a `CapabilityProvider`. There is no universal
/// `handle_event`; modules publish typed services and consumers look them up
/// by type.
pub trait CapabilityProvider: Send + Sync + 'static {}
impl<T: Send + Sync + 'static> CapabilityProvider for T {}

/// Typed capability markers over the existing engine contracts. Implemented
/// for the `Arc<dyn ...>` forms the composition root actually holds, so the
/// module host can register and look them up without a catch-all event type.
pub trait ContextService: CapabilityProvider {}
impl ContextService for std::sync::Arc<dyn ContextEngine> {}

pub trait ModelProvider: CapabilityProvider {}
impl ModelProvider for std::sync::Arc<dyn ModelTransport> {}

pub trait ToolProvider: CapabilityProvider {}
impl ToolProvider for std::sync::Arc<dyn ToolDispatcher> {}

pub trait ApprovalPolicy: CapabilityProvider {}
impl ApprovalPolicy for std::sync::Arc<dyn ApprovalGate> {}

pub trait EventStore: CapabilityProvider {}
impl EventStore for std::sync::Arc<dyn EventJournal> {}

/// Lookup result for an optional capability (e.g. the event journal is
/// optional in the prototype).
pub type CapabilityLookup<T> = AgentResult<T>;

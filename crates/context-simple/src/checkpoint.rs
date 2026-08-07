use agent_contracts::{AgentError, AgentResult};
use serde_json::Value;

use crate::engine::State;

/// Export the runtime state (items, focus, counters, queues) as JSON, kept
/// separate from the event journal.
pub(crate) fn serialize(state: &State) -> AgentResult<Value> {
    serde_json::to_value(state)
        .map_err(|e| AgentError::Context(format!("checkpoint serialize: {e}")))
}

/// Replace runtime state from a previously exported checkpoint.
pub(crate) fn deserialize(data: Value) -> AgentResult<State> {
    serde_json::from_value(data)
        .map_err(|e| AgentError::Context(format!("checkpoint restore: {e}")))
}

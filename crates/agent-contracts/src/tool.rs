use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentResult, CancellationToken, RunId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolRisk {
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: ToolRisk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub summary: String,
    /// Bounded content intended for the next model turn.
    pub model_content: String,
    /// Raw/large output should live here instead of in the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub run_id: RunId,
    pub call: ToolCall,
    /// Cooperative cancellation handle for this execution (kill long-running
    /// processes, abort expensive searches). Not serialized.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// The current tool surface. MUST be pure: a model round reads it for
    /// the budget, the prompt and tool-call validation, so the surface has
    /// to stay stable within a round.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Explicit lifecycle maintenance safe point, called by the runtime
    /// once per model round. Dispatchers with mutable tool lifecycles
    /// (idle aging, unloading) run their GC here — never inside `specs()`.
    fn gc(&self) {}

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput>;
}

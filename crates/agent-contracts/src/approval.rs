use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, ToolCall, ToolSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn authorize(&self, call: &ToolCall, spec: &ToolSpec) -> AgentResult<ApprovalDecision>;
}

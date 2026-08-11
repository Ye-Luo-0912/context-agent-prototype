use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, CancellationToken, ToolCall, ToolSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Decides whether one tool call may run. The `cancel` token lets a
/// waiting gate (interactive prompt, standing-grant negotiation) abort when
/// the operation itself is cancelled — a cancelled turn must not leave a
/// pending approval request behind, and a gate that waits (up to a bounded
/// answer timeout) must stop waiting the moment its caller is gone.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision>;
}

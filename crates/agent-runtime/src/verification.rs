//! Host-declared exact verification executed by Runtime, not by the model.
//!
//! The completion gate may auto-refresh a stale proof only through this
//! narrow contract: the composition root injects the concrete executor
//! (the process lane over the same recipe table the `verify.run` tool
//! uses). The verifier runs outside the model tool surface — no approval
//! gate, no operation admission, no model-visible tool result — and its
//! PASS is only trusted when the host attribution for the same recipe
//! agrees on the exact verification identity.

use agent_contracts::{AgentResult, RunId, TaskId};

/// One bounded request to run the host-declared exact verifier for a
/// recipe. The fence pre-state is included so the executor and Runtime
/// agree on what world the check ran against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofVerifierRequest {
    pub run_id: RunId,
    pub task_id: TaskId,
    /// Host-registered recipe id (`verify.run` names the same table).
    pub recipe_id: String,
    /// Fence pre-state: the PASS is discarded unless the runtime still
    /// holds these revisions after the run.
    pub verification_revision: u64,
    pub directive_revision: u64,
    pub workspace_revision: u64,
}

/// Result of one host-side exact verification run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofVerifierOutcome {
    /// Exit success of the declared verification process.
    pub ok: bool,
    /// Bounded human summary of the run.
    pub summary: String,
    /// Exact verifier identity derived from the host binding material.
    /// Runtime fails closed when this does not match the host attribution
    /// for the same recipe.
    pub verification_identity: String,
}

/// Executes one host-declared verification recipe for the completion-gate
/// proof-refresh transaction. `None` in services disables the transaction.
#[async_trait::async_trait]
pub trait ProofVerifier: Send + Sync {
    async fn verify_exact(
        &self,
        request: ProofVerifierRequest,
    ) -> AgentResult<ProofVerifierOutcome>;
}

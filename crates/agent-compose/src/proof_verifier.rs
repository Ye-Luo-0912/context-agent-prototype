//! 宿主 exact 证明刷新适配：组合根把 tool-runtime 的 recipe 证明 runner
//! 实现成 runtime 的 `ProofVerifier` 契约。该 lane 运行在模型工具面之外
//! ——无审批门、无操作准入、无模型可见工具结果——PASS 只有当身份与同一
//! recipe 的宿主归因一致时才会被 completion gate 信任（fail closed 由
//! gate 侧的 fence 负责，这里只忠实映射一次宿主运行）。

use agent_contracts::{AgentResult, CancellationToken};
use agent_runtime::{ProofVerifier, ProofVerifierOutcome, ProofVerifierRequest};
use tool_runtime::RecipeProofRunner;

/// Maps one host proof run onto the runtime's verifier contract.
pub struct HostProofVerifier {
    runner: RecipeProofRunner,
}

impl HostProofVerifier {
    pub fn new(runner: RecipeProofRunner) -> Self {
        Self { runner }
    }
}

#[async_trait::async_trait]
impl ProofVerifier for HostProofVerifier {
    async fn verify_exact(
        &self,
        request: ProofVerifierRequest,
    ) -> AgentResult<ProofVerifierOutcome> {
        let run = self
            .runner
            .verify_exact(request.run_id, &request.recipe_id, CancellationToken::new())
            .await?;
        Ok(ProofVerifierOutcome {
            ok: run.ok,
            summary: run.summary,
            verification_identity: run.verification_identity,
        })
    }
}

//! Host-trusted execution of one registered verification recipe.
//!
//! The composition root (`agent-compose`) owns this lane and injects it into
//! the runtime completion gate as the proof-refresh executor. Unlike the
//! model-facing `verify.run` tool there is no approval gate, no operation
//! admission and no model-visible tool result: the host itself is the
//! authority. The lane still runs through the process runner's bounded
//! execution (argv/env/cwd bounds, program preflight, timeout, whole-tree
//! kill, bounded capture and artifact seal) and the actual argv must still
//! be covered by the same recipe host policy, so a runner/table drift fails
//! closed before spawning. The minted identity comes from the same
//! per-recipe material the dispatcher attribution stamps, so a host PASS
//! can never disagree with the model-lane attribution for the same recipe.

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ContentDigest, RunId, ToolOutcome,
    VerificationReuse,
};
use agent_workspace::Workspace;
use serde_json::json;

use crate::VERIFY_RUN_TOOL_NAME;
use crate::tools::{ProcessArgs, ProcessInvocation, ProcessRunTool};
use crate::verification::{VerificationRecipes, recipe_exact_identity};

/// Runs host-owned verification recipes for the completion-gate
/// proof-refresh transaction.
pub struct RecipeProofRunner {
    workspace: Workspace,
    recipes: Arc<VerificationRecipes>,
    process: ProcessRunTool,
}

/// Bounded result of one host verification run.
#[derive(Debug)]
pub struct RecipeProofRun {
    /// Exit success of the declared verification process.
    pub ok: bool,
    /// Bounded human summary of the run.
    pub summary: String,
    /// `sha256(trim)` of the recipe identity material — byte-identical to
    /// the dispatcher attribution identity for the same recipe/world.
    pub verification_identity: String,
}

impl RecipeProofRunner {
    /// Constructs only when the recipe table carries a host-effect policy;
    /// an empty or unregistered table refuses to exist rather than minting
    /// proof without authority.
    pub fn new(workspace: Workspace, recipes: Arc<VerificationRecipes>) -> Option<Self> {
        recipes.host_policy()?;
        let process = ProcessRunTool::new(workspace.clone());
        Some(Self {
            workspace,
            recipes,
            process,
        })
    }

    /// The exact identity for one recipe without executing it — the digest
    /// the dispatcher attribution stamps in this world. `None` for unknown
    /// recipes, non-exact reuse, or a world where exact equivalence cannot
    /// be captured completely.
    pub fn exact_identity(&self, recipe_id: &str) -> Option<String> {
        let recipe = self.recipes.get(recipe_id)?;
        let identity = recipe_exact_identity(&self.recipes, recipe, &self.workspace)?;
        Some(ContentDigest::sha256_bytes(identity.material.trim().as_bytes()).to_string())
    }

    /// Execute one exact current-world recipe synchronously. Non-exact
    /// recipes and unknown ids are refused; an incomputable identity fails
    /// closed so no bare process success can ever mint a proof receipt.
    pub async fn verify_exact(
        &self,
        run_id: RunId,
        recipe_id: &str,
        cancel: CancellationToken,
    ) -> AgentResult<RecipeProofRun> {
        let recipe_id = recipe_id.trim();
        let recipe = self.recipes.get(recipe_id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("unknown verification recipe '{recipe_id}'"))
        })?;
        if recipe.reuse != VerificationReuse::ExactCurrentWorld {
            return Err(AgentError::InvalidRequest(format!(
                "verification recipe '{recipe_id}' is not an exact current-world recipe"
            )));
        }
        let identity = recipe_exact_identity(&self.recipes, recipe, &self.workspace).ok_or_else(
            || {
                AgentError::InvalidRequest(format!(
                    "exact identity for verification recipe '{recipe_id}' is not computable in this world; refusing host proof"
                ))
            },
        )?;
        let verification_identity =
            ContentDigest::sha256_bytes(identity.material.trim().as_bytes()).to_string();
        // The runner only exists with a policy; the check below is belt and
        // suspenders so the intent coverage gate always has a table.
        let policy = self.recipes.host_policy().ok_or_else(|| {
            AgentError::InvalidRequest("verification recipes register no host policy".into())
        })?;
        let outcome = self
            .process
            .execute_invocation(ProcessInvocation {
                tool_name: VERIFY_RUN_TOOL_NAME,
                run_id,
                call_id: "host-proof",
                authority_arguments: &json!({ "recipe_id": recipe_id }),
                authority_policy: &policy,
                args: ProcessArgs {
                    argv: recipe.argv.clone(),
                    cwd: recipe.cwd.clone(),
                    env: recipe
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                    timeout_ms: recipe.timeout_ms,
                },
                effect_context: None,
                cancel,
                host_trusted: true,
            })
            .await?;
        let ToolOutcome::Value(output) = outcome else {
            return Err(AgentError::Tool(
                "host verification returned a non-value outcome".into(),
            ));
        };
        Ok(RecipeProofRun {
            ok: output.ok,
            summary: output.summary,
            verification_identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VerificationRecipe;

    /// An argv that echoes its argument and exits zero.
    fn echo_recipe() -> VerificationRecipe {
        #[cfg(windows)]
        let argv = vec!["cmd".into(), "/C".into(), "echo".into(), "trusted".into()];
        #[cfg(not(windows))]
        let argv = vec!["echo".into(), "trusted".into()];
        VerificationRecipe::new("echo.trusted", "Echo trusted marker", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse()
    }

    /// An argv that exits non-zero.
    fn fail_recipe() -> VerificationRecipe {
        #[cfg(windows)]
        let argv = vec![
            "cmd".into(),
            "/C".into(),
            "exit".into(),
            "/b".into(),
            "1".into(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["sh".into(), "-c".into(), "exit 1".into()];
        VerificationRecipe::new("fail.probe", "Exit non-zero", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse()
    }

    /// An argv that outlives the minimum recipe timeout.
    fn sleepy_recipe() -> VerificationRecipe {
        #[cfg(windows)]
        let argv = vec![
            "cmd".into(),
            "/C".into(),
            "ping".into(),
            "-n".into(),
            "30".into(),
            "127.0.0.1".into(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["sh".into(), "-c".into(), "sleep 30".into()];
        let mut recipe = VerificationRecipe::new("sleep.long", "Sleep past timeout", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse();
        recipe.timeout_ms = 100;
        recipe
    }

    async fn runner_with(
        recipes: Vec<VerificationRecipe>,
    ) -> (RecipeProofRunner, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(recipes).unwrap());
        let runner = RecipeProofRunner::new(workspace, recipes).unwrap();
        (runner, dir)
    }

    /// A PASS mints a bounded summary and the identity the dispatcher
    /// attribution would stamp for the same recipe.
    #[tokio::test]
    async fn pass_mints_the_exact_identity() {
        let (runner, _dir) = runner_with(vec![echo_recipe()]).await;
        let run = runner
            .verify_exact(RunId::new(), "echo.trusted", CancellationToken::new())
            .await
            .unwrap();
        assert!(run.ok, "{}", run.summary);
        assert_eq!(
            run.verification_identity,
            runner.exact_identity("echo.trusted").unwrap()
        );
        assert!(!run.verification_identity.is_empty());
        assert_eq!(run.verification_identity.len(), 64);
    }

    /// A FAIL is reported honestly with its (world-derived) identity — the
    /// gate decides what a failure means, the lane only reports facts.
    #[tokio::test]
    async fn failure_is_reported_with_identity() {
        let (runner, _dir) = runner_with(vec![fail_recipe()]).await;
        let run = runner
            .verify_exact(RunId::new(), "fail.probe", CancellationToken::new())
            .await
            .unwrap();
        assert!(!run.ok, "{}", run.summary);
        assert_eq!(
            run.verification_identity,
            runner.exact_identity("fail.probe").unwrap()
        );
    }

    /// Unknown ids are refused before any process resolution.
    #[tokio::test]
    async fn unknown_recipe_is_refused_before_spawn() {
        let (runner, _dir) = runner_with(vec![echo_recipe()]).await;
        let error = runner
            .verify_exact(RunId::new(), "missing", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown verification recipe"));
    }

    /// Only exact current-world recipes can mint proof; a task-scoped
    /// runner never masquerades as one.
    #[tokio::test]
    async fn non_exact_recipe_is_refused() {
        let mut plain = echo_recipe();
        plain.reuse = VerificationReuse::TaskScoped;
        let (runner, _dir) = runner_with(vec![plain]).await;
        let error = runner
            .verify_exact(RunId::new(), "echo.trusted", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not an exact current-world recipe"),
            "{error}"
        );
        assert!(runner.exact_identity("echo.trusted").is_none());
    }

    /// Without any Core effect identity the lane still times out and kills
    /// the whole tree instead of leaking a long-lived child.
    #[tokio::test]
    async fn timeout_kills_the_tree_without_core_identity() {
        let (runner, _dir) = runner_with(vec![sleepy_recipe()]).await;
        let start = std::time::Instant::now();
        let run = runner
            .verify_exact(RunId::new(), "sleep.long", CancellationToken::new())
            .await
            .unwrap();
        assert!(!run.ok, "{}", run.summary);
        assert!(run.summary.contains("timed out"), "{}", run.summary);
        assert!(
            start.elapsed().as_secs() < 15,
            "the timed-out child must not be left running"
        );
        assert!(!run.verification_identity.is_empty());
    }
}

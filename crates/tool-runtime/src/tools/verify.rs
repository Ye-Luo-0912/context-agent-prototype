//! `verify.run` — execute one host-registered verification recipe.

use std::collections::HashMap;
use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, HostToolPolicy, RunId, ToolOutcome, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{VERIFY_RUN_TOOL_NAME, VerificationRecipes};

use super::Tool;
use super::process::{ProcessArgs, ProcessInvocation, ProcessRunTool};

pub(crate) struct VerificationRunTool {
    recipes: Arc<VerificationRecipes>,
    authority_policy: HostToolPolicy,
    process: ProcessRunTool,
}

impl VerificationRunTool {
    pub(crate) fn new(workspace: Workspace, recipes: Arc<VerificationRecipes>) -> Option<Self> {
        let authority_policy = recipes.host_policy()?;
        Some(Self {
            recipes,
            authority_policy,
            process: ProcessRunTool::new(workspace),
        })
    }
}

#[derive(Deserialize)]
struct VerifyArgs {
    recipe_id: String,
}

#[async_trait]
impl Tool for VerificationRunTool {
    fn spec(&self) -> ToolSpec {
        let ids: Vec<&str> = self
            .recipes
            .as_slice()
            .iter()
            .map(|recipe| recipe.id.as_str())
            .collect();
        let catalog = self
            .recipes
            .as_slice()
            .iter()
            .map(|recipe| format!("{}: {}", recipe.id, recipe.description))
            .collect::<Vec<_>>()
            .join("; ");
        ToolSpec {
            name: VERIFY_RUN_TOOL_NAME.into(),
            description: format!(
                "Run one trusted project verification recipe. The host owns argv, cwd and environment; choose only a recipe_id. Recipe ids are values for verify.run, not tool names and never need capability.manage load. Successful current-world PASS may be reused without another process. Recipes: {catalog}"
            ),
            input_schema: json!({
                "type": "object",
                "required": ["recipe_id"],
                "additionalProperties": false,
                "properties": {
                    "recipe_id": {
                        "type": "string",
                        "enum": ids,
                        "description": "Exact host-registered recipe id; pass it here, never to capability.manage"
                    }
                }
            }),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::Verify],
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: VerifyArgs = serde_json::from_value(arguments.clone())
            .map_err(|error| AgentError::InvalidRequest(format!("verify.run args: {error}")))?;
        let recipe = self.recipes.get(args.recipe_id.trim()).ok_or_else(|| {
            AgentError::InvalidRequest(format!(
                "unknown verification recipe '{}'",
                args.recipe_id.trim()
            ))
        })?;
        let process_args = ProcessArgs {
            argv: recipe.argv.clone(),
            cwd: recipe.cwd.clone(),
            env: recipe
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<HashMap<_, _>>(),
            timeout_ms: recipe.timeout_ms,
        };
        let outcome = self
            .process
            .execute_invocation(ProcessInvocation {
                tool_name: VERIFY_RUN_TOOL_NAME,
                run_id,
                call_id,
                authority_arguments: &arguments,
                authority_policy: &self.authority_policy,
                args: process_args,
                effect_context,
                cancel,
            })
            .await?;
        Ok(match outcome {
            ToolOutcome::Value(mut output) => {
                output.summary = format!(
                    "verification {} {}: {}",
                    recipe.id,
                    if output.ok { "passed" } else { "failed" },
                    output.summary
                );
                if let Some(metadata) = output.metadata.as_object_mut() {
                    metadata.insert("recipe_id".into(), json!(recipe.id));
                    metadata.insert("recipe_revision".into(), json!(recipe.revision));
                    // Exact recipes assert that build/cache output is confined
                    // to ignored/runtime paths and source inputs are read-only.
                    // General test runners remain conservative Unknown
                    // mutations even though their result is typed Verify.
                    metadata.insert("mutates_workspace".into(), json!(!recipe.source_read_only));
                    metadata.insert("verification".into(), json!(true));
                }
                ToolOutcome::Value(output)
            }
            other => other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_recipe() -> crate::VerificationRecipe {
        #[cfg(windows)]
        let argv = vec![
            "cmd".into(),
            "/C".into(),
            "echo".into(),
            "trusted-recipe".into(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["echo".into(), "trusted-recipe".into()];
        crate::VerificationRecipe::new("echo.trusted", "Echo trusted marker", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse()
    }

    #[tokio::test]
    async fn schema_distinguishes_recipe_values_from_tool_names() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![echo_recipe()]).unwrap());
        let spec = VerificationRunTool::new(workspace, recipes).unwrap().spec();
        assert!(spec.description.contains("not tool names"));
        assert!(
            spec.description
                .contains("never need capability.manage load")
        );
        assert_eq!(
            spec.input_schema["properties"]["recipe_id"]["enum"][0],
            "echo.trusted"
        );
    }

    #[tokio::test]
    async fn model_selects_id_but_cannot_replace_recipe_argv() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![echo_recipe()]).unwrap());
        let tool = VerificationRunTool::new(workspace, recipes).unwrap();
        let run_id = RunId::new();
        let arguments = json!({
            "recipe_id": "echo.trusted",
            "argv": ["definitely-not-the-command"]
        });
        let context = crate::tools::test_process_effect_context(
            run_id,
            "call-verify",
            VERIFY_RUN_TOOL_NAME,
            &arguments,
        );
        let output = tool
            .execute(
                run_id,
                "call-verify",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = output else {
            panic!("verify.run is a non-transactional process tool")
        };
        assert!(output.ok, "{}", output.model_content);
        assert!(output.model_content.contains("trusted-recipe"));
        assert!(!output.model_content.contains("definitely-not-the-command"));
        assert_eq!(output.tool_name, VERIFY_RUN_TOOL_NAME);
        assert_eq!(output.metadata["recipe_id"], "echo.trusted");
    }

    #[tokio::test]
    async fn unknown_recipe_fails_before_process_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![echo_recipe()]).unwrap());
        let tool = VerificationRunTool::new(workspace, recipes).unwrap();
        let run_id = RunId::new();
        let arguments = json!({"recipe_id": "missing"});
        let context = crate::tools::test_process_effect_context(
            run_id,
            "call-verify",
            VERIFY_RUN_TOOL_NAME,
            &arguments,
        );
        let error = tool
            .execute(
                run_id,
                "call-verify",
                arguments,
                Some(context),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown verification recipe"));
    }
}

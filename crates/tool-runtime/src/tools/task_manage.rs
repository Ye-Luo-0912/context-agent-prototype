//! `task.manage` — the model's bounded autonomous progress proposal.
//!
//! The tool does no work itself: it packages the proposal (a compare-and-swap
//! base anchor revision plus optional interpretation / plan / open-loops /
//! one next-action) as a typed `RuntimeDirective::UpdateTaskProgress`. The
//! runtime applies it synchronously at the operation-commit point and writes
//! the authoritative CAS outcome back into the model-visible result, so a
//! stale revision is retryable in the very next round.
//!
//! User authority stays structurally out of reach: the argument schema has
//! no goal/constraints fields and unknown keys are rejected, while goal and
//! constraint changes keep flowing through the existing boundary/approval
//! path. The catalog-cold default means ordinary turns never see this tool;
//! explicit long-task runs add it as a task requirement.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, MAX_TASK_ANCHOR_ITEM_CHARS,
    MAX_TASK_ANCHOR_LIST_ITEMS, MAX_TASK_ANCHOR_TEXT_CHARS, RunId, RuntimeDirective,
    TaskProgressProposal, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;

pub struct TaskManageTool;

impl TaskManageTool {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManageArgs {
    base_anchor_revision: u64,
    #[serde(default)]
    current_interpretation: Option<String>,
    #[serde(default)]
    plan_progress: Option<Vec<String>>,
    #[serde(default)]
    open_loops: Option<Vec<String>>,
    #[serde(default)]
    next_action: Option<String>,
}

/// Fail fast on the same bounds the runtime re-validates during the CAS, so
/// a malformed proposal never reaches the task transaction.
fn validate_proposal(args: &ManageArgs) -> AgentResult<()> {
    let check_text = |name: &str, value: &str, limit: usize| {
        if value.chars().count() > limit {
            return Err(AgentError::InvalidRequest(format!(
                "{name} is limited to {limit} chars"
            )));
        }
        Ok(())
    };
    if let Some(value) = &args.current_interpretation {
        check_text("current_interpretation", value, MAX_TASK_ANCHOR_TEXT_CHARS)?;
    }
    for (name, list) in [
        ("plan_progress", &args.plan_progress),
        ("open_loops", &args.open_loops),
    ] {
        if let Some(list) = list {
            if list.len() > MAX_TASK_ANCHOR_LIST_ITEMS {
                return Err(AgentError::InvalidRequest(format!(
                    "{name} is limited to {MAX_TASK_ANCHOR_LIST_ITEMS} items"
                )));
            }
            for item in list {
                check_text(name, item, MAX_TASK_ANCHOR_ITEM_CHARS)?;
            }
        }
    }
    if let Some(value) = &args.next_action {
        check_text("next_action", value, MAX_TASK_ANCHOR_ITEM_CHARS)?;
    }
    Ok(())
}

#[async_trait]
impl Tool for TaskManageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task.manage".into(),
            description: "Record bounded progress on the long-lived task: current interpretation, plan progress, open loops and one replaceable next_action. Compare-and-swap on base_anchor_revision (read it from PERSISTENT TASK STATE); a stale revision is refused without state change and can be retried from the reported current revision. This never edits the user goal or constraints.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["base_anchor_revision"],
                "properties": {
                    "base_anchor_revision": {"type": "integer", "minimum": 0, "description": "Anchor revision this proposal was written against"},
                    "current_interpretation": {"type": "string", "description": "Replacement current understanding of the goal"},
                    "plan_progress": {"type": "array", "items": {"type": "string"}, "description": "Replacement ordered plan-progress list"},
                    "open_loops": {"type": "array", "items": {"type": "string"}, "description": "Replacement open-loop list"},
                    "next_action": {"type": "string", "description": "Single replaceable suggested next step"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ManageArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("task.manage args: {e}")))?;
        validate_proposal(&args)?;

        // Only requested fields move; omitted fields keep their values, so
        // a partial proposal never resets sibling autonomous state.
        let touched = [
            args.current_interpretation.is_some(),
            args.plan_progress.is_some(),
            args.open_loops.is_some(),
            args.next_action.is_some(),
        ]
        .iter()
        .filter(|touched| **touched)
        .count();
        let proposal = TaskProgressProposal {
            base_anchor_revision: args.base_anchor_revision,
            current_interpretation: args.current_interpretation,
            plan_progress: args.plan_progress,
            open_loops: args.open_loops,
            next_action: args.next_action,
        };
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "task.manage".into(),
                ok: true,
                summary: format!(
                    "progress proposed ({touched} field(s)); the runtime applies it against anchor revision {}",
                    proposal.base_anchor_revision
                ),
                model_content: String::new(),
                artifact_ref: None,
                metadata: json!({
                    "base_anchor_revision": proposal.base_anchor_revision,
                    "requested_fields": touched,
                }),
            }
            .with_native_execution_facts(super::builtin_bound(false)),
            directive: RuntimeDirective::UpdateTaskProgress(proposal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn packages_a_bounded_progress_directive() {
        let tool = TaskManageTool::new();
        let completion = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "base_anchor_revision": 4,
                    "plan_progress": ["config parsed"],
                    "next_action": "implement delay saturation"
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        match completion {
            ToolOutcome::RuntimeDirective { output, directive } => {
                assert!(output.ok);
                assert_eq!(output.tool_name, "task.manage");
                assert!(output.model_content.is_empty(), "no body may be echoed");
                match directive {
                    RuntimeDirective::UpdateTaskProgress(proposal) => {
                        assert_eq!(proposal.base_anchor_revision, 4);
                        assert_eq!(
                            proposal.plan_progress.as_deref(),
                            Some(&["config parsed".to_string()][..])
                        );
                        assert_eq!(
                            proposal.next_action.as_deref(),
                            Some("implement delay saturation")
                        );
                        assert!(proposal.current_interpretation.is_none());
                        assert!(proposal.open_loops.is_none());
                    }
                    other => panic!("expected a progress directive, got {other:?}"),
                }
            }
            other => panic!("expected a runtime directive, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_fields_are_rejected_instead_of_silently_ignored() {
        let tool = TaskManageTool::new();
        let refused = tool
            .execute(
                RunId::new(),
                "c",
                json!({"base_anchor_revision": 1, "constraints": ["no network"]}),
                None,
                CancellationToken::new(),
            )
            .await
            .expect_err("user-authority fields must not pass through");
        assert!(refused.to_string().contains("task.manage args"));
    }

    #[tokio::test]
    async fn oversized_fields_fail_before_the_runtime_sees_them() {
        let tool = TaskManageTool::new();
        let oversized = "x".repeat(MAX_TASK_ANCHOR_ITEM_CHARS + 1);
        let refused = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "base_anchor_revision": 0,
                    "next_action": oversized
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("limited to"));

        let too_many = vec!["loop".to_string(); MAX_TASK_ANCHOR_LIST_ITEMS + 1];
        let refused = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "base_anchor_revision": 0,
                    "open_loops": too_many
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("limited to"));
    }

    #[tokio::test]
    async fn missing_base_revision_is_a_clear_error() {
        let tool = TaskManageTool::new();
        let refused = tool
            .execute(
                RunId::new(),
                "c",
                json!({"next_action": "x"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("task.manage args"));
    }

    #[test]
    fn spec_stays_readonly_and_named() {
        let spec = TaskManageTool::new().spec();
        assert_eq!(spec.name, "task.manage");
        assert!(matches!(spec.risk, ToolRisk::ReadOnly));
    }
}

//! `task.complete` — the model's structured completion control.
//!
//! The tool does no work itself: it packages the model's completion
//! proposal (a bounded summary plus bounded `artifact://` refs) as a typed
//! `RuntimeDirective::CompleteTask` that the runtime validates and commits
//! as the active task's `CompletionRecord` at the turn's safe point — after
//! the turn commits, through the same CTX-10 transaction the `/done` path
//! uses. Tools never touch task authority directly (the runtime owns the
//! task table); this tool only names *what* the model wants completed.

use agent_contracts::{
    AgentError, AgentResult, ArtifactLocator, CancellationToken, CompletionProposal,
    MAX_COMPLETION_ARTIFACTS, MAX_COMPLETION_REF_CHARS, MAX_COMPLETION_SUMMARY_CHARS, RunId,
    RuntimeDirective, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;

pub struct TaskCompleteTool;

impl TaskCompleteTool {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct CompleteArgs {
    summary: String,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[async_trait]
impl Tool for TaskCompleteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task.complete".into(),
            description: "Propose task completion (summary + optional artifact:// refs).".into(),
            input_schema: json!({
                "type": "object",
                "required": ["summary"],
                "properties": {
                    "summary": {"type": "string", "description": "Completion summary (bounded)"},
                    "artifacts": {"type": "array", "items": {"type": "string"}, "description": "Optional artifact:// refs produced by the completion (bounded)"}
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
        let args: CompleteArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("task.complete args: {e}")))?;

        // Bounds are enforced here (fail fast with a clear tool error) and
        // re-validated by the runtime before the proposal is accepted, so
        // a malformed call can never reach the completion transaction.
        if args.summary.trim().is_empty() {
            return Err(AgentError::InvalidRequest(
                "completion summary must not be empty".into(),
            ));
        }
        if args.summary.chars().count() > MAX_COMPLETION_SUMMARY_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "completion summary is limited to {MAX_COMPLETION_SUMMARY_CHARS} chars"
            )));
        }
        if args.artifacts.len() > MAX_COMPLETION_ARTIFACTS {
            return Err(AgentError::InvalidRequest(format!(
                "task.complete is limited to {MAX_COMPLETION_ARTIFACTS} artifact refs"
            )));
        }
        for artifact in &args.artifacts {
            ArtifactLocator::parse_sealed(artifact)?;
            if artifact.chars().count() > MAX_COMPLETION_REF_CHARS {
                return Err(AgentError::InvalidRequest(format!(
                    "completion artifact ref is limited to {MAX_COMPLETION_REF_CHARS} chars"
                )));
            }
        }

        let proposal = CompletionProposal {
            summary: args.summary,
            artifacts: args.artifacts,
        };
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "task.complete".into(),
                ok: true,
                summary: "completion proposed — the runtime commits it when this turn finalizes"
                    .into(),
                model_content: format!(
                    "completion proposed; the runtime validates and commits the typed record at the turn's safe point.\nsummary: {}",
                    proposal.summary
                ),
                artifact_ref: None,
                metadata: json!({
                    "proposal": {
                        "summary_chars": proposal.summary.chars().count(),
                        "artifacts": proposal.artifacts.len(),
                    },
                }),
            },
            directive: RuntimeDirective::CompleteTask(proposal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ContentDigest, ToolExecutionRequest};
    use serde_json::json;

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "task.complete".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    fn directive(outcome: ToolOutcome) -> (ToolOutput, RuntimeDirective) {
        match outcome {
            ToolOutcome::RuntimeDirective { output, directive } => (output, directive),
            _ => panic!("task.complete must attach a runtime directive"),
        }
    }

    #[tokio::test]
    async fn packages_a_bounded_completion_proposal() {
        let tool = TaskCompleteTool::new();
        let run_id = RunId::new();
        let evidence = ArtifactLocator::sealed(run_id, "grep", ContentDigest::sha256_bytes(b"out"))
            .unwrap()
            .to_string();
        let request = request(
            run_id,
            json!({"summary": "task done", "artifacts": [evidence]}),
        );
        let (output, directive) = {
            let outcome = tool
                .execute(run_id, "c", request.call.arguments, None, request.cancel)
                .await
                .unwrap();
            directive(outcome)
        };
        assert!(output.ok);
        let RuntimeDirective::CompleteTask(proposal) = directive else {
            panic!("must be a completion directive");
        };
        assert_eq!(proposal.summary, "task done");
        assert_eq!(proposal.artifacts.len(), 1);
    }

    #[tokio::test]
    async fn refuses_oversized_and_malformed_proposals() {
        let tool = TaskCompleteTool::new();
        let run_id = RunId::new();

        let empty = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"summary": "   "})).call.arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(empty.is_err(), "an empty summary must be refused");

        let oversized = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"summary": "x".repeat(MAX_COMPLETION_SUMMARY_CHARS + 1)}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(oversized.is_err(), "an oversized summary must be refused");

        let not_artifact = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"summary": "done", "artifacts": ["https://example.com/x"]}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(not_artifact.is_err(), "non-artifact refs must be refused");

        let too_many = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"summary": "done", "artifacts": (0..=MAX_COMPLETION_ARTIFACTS).map(|i| format!("artifact://a/{i}")).collect::<Vec<_>>()}),
                )
                .call.arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(too_many.is_err(), "too many artifacts must be refused");
    }
}

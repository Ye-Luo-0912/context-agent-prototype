//! A scripted model transport for deterministic harness runs.
//!
//! The live M15 evaluation needs a provider that accepts tool calls; the
//! current endpoint rejects them. This transport is the deterministic
//! stand-in: the Nth model request returns the Nth scripted tool call, and
//! once the script is exhausted the model answers with a fixed completion
//! message. That drives the *real* tool surface through the *real* runtime
//! (workspace confinement, prepared effects, generation fence, cost
//! accounting), so the harness itself is proven end to end without a
//! provider. It is deliberately not a model-quality test.

use std::sync::atomic::{AtomicUsize, Ordering};

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, ModelUsage,
    ToolCall, tokens,
};

/// One scripted model. `steps` is the tool-call script, `done` the final
/// completion message after the script runs out.
pub struct ScriptedModel {
    steps: Vec<ToolCall>,
    done: String,
    requests: AtomicUsize,
}

impl ScriptedModel {
    pub fn new(steps: Vec<ToolCall>, done: impl Into<String>) -> Self {
        Self {
            steps,
            done: done.into(),
            requests: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for ScriptedModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: false,
            tool_calls: true,
            max_output_tokens: 1024,
            context_window: None,
        }
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let index = self.requests.fetch_add(1, Ordering::SeqCst);
        // Report the input the scripted model "saw": the messages (content
        // plus serialized tool calls) and the tool schemas of this request,
        // priced with the same estimator the engines use. This makes the
        // fixture path's ModelUsed accounting real, so a cross-engine
        // comparison can measure how much context each engine feeds a
        // request without a provider.
        let input_tokens = request
            .messages
            .iter()
            .map(|message| {
                tokens::approx_tokens(&message.content)
                    + message
                        .tool_calls
                        .iter()
                        .map(|call| {
                            tokens::approx_tokens(&call.name)
                                + tokens::approx_tokens(&call.arguments.to_string())
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
            + request
                .tools
                .iter()
                .map(|spec| {
                    tokens::approx_tokens(&spec.name)
                        + tokens::approx_tokens(&spec.description)
                        + tokens::approx_tokens(&spec.input_schema.to_string())
                })
                .sum::<usize>();
        let usage = ModelUsage {
            input_tokens: Some(input_tokens as u64),
            output_tokens: Some(if index < self.steps.len() {
                tokens::approx_tokens(&self.steps[index].arguments.to_string()) as u64
            } else {
                tokens::approx_tokens(&self.done) as u64
            }),
        };
        if let Some(call) = self.steps.get(index) {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![call.clone()],
                usage,
            })
        } else {
            Ok(ModelOutput {
                content: self.done.clone(),
                tool_calls: Vec::new(),
                usage,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn scripted_model_emits_steps_then_done() {
        let model = ScriptedModel::new(
            vec![ToolCall {
                id: "c1".into(),
                name: "fs.read".into(),
                arguments: json!({"path": "src/main.rs"}),
            }],
            "done",
        );
        let request = ModelRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            metadata: serde_json::Value::Null,
            cancel: agent_contracts::CancellationToken::new(),
        };
        let first = model.complete(request.clone()).await.unwrap();
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "fs.read");
        assert!(first.content.is_empty());

        let second = model.complete(request).await.unwrap();
        assert_eq!(second.content, "done");
        assert!(second.tool_calls.is_empty());
    }
}

//! A scripted model transport for deterministic harness runs.
//!
//! CI fixture runs must not depend on a live provider. This transport is
//! the stand-in: the Nth model request returns the Nth scripted tool call,
//! and once the script is exhausted the model answers with a fixed
//! completion message. That drives the *real* tool surface through the
//! *real* runtime (workspace confinement, prepared effects, generation
//! fence, cost accounting). It is deliberately not a model-quality test.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    /// 每个 tool 之后先回一条无 tool 的 completion，结束当前 turn。
    /// 多轮 live 题的脚本化对照用：一轮用户输入对应一次工具。
    one_tool_per_turn: bool,
    emit_done_next: AtomicBool,
}

impl ScriptedModel {
    pub fn new(steps: Vec<ToolCall>, done: impl Into<String>) -> Self {
        Self {
            steps,
            done: done.into(),
            requests: AtomicUsize::new(0),
            one_tool_per_turn: false,
            emit_done_next: AtomicBool::new(false),
        }
    }

    pub fn one_tool_per_turn(mut self) -> Self {
        self.one_tool_per_turn = true;
        self
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
        let input_tokens = price_request(&request);
        if self.one_tool_per_turn && self.emit_done_next.swap(false, Ordering::SeqCst) {
            return Ok(done_output(&self.done, input_tokens));
        }
        let index = self.requests.fetch_add(1, Ordering::SeqCst);
        if let Some(call) = self.steps.get(index) {
            if self.one_tool_per_turn {
                self.emit_done_next.store(true, Ordering::SeqCst);
            }
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![call.clone()],
                usage: ModelUsage {
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(tokens::approx_tokens(&call.arguments.to_string()) as u64),
                    ..Default::default()
                },
            })
        } else {
            Ok(done_output(&self.done, input_tokens))
        }
    }
}

fn price_request(request: &ModelRequest) -> u64 {
    let tokens = request
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
    tokens as u64
}

fn done_output(done: &str, input_tokens: u64) -> ModelOutput {
    ModelOutput {
        content: done.to_string(),
        tool_calls: Vec::new(),
        usage: ModelUsage {
            input_tokens: Some(input_tokens),
            output_tokens: Some(tokens::approx_tokens(done) as u64),
            ..Default::default()
        },
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

    #[tokio::test]
    async fn one_tool_per_turn_inserts_a_done_between_steps() {
        let model = ScriptedModel::new(
            vec![
                ToolCall {
                    id: "c1".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "a"}),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "fs.write".into(),
                    arguments: json!({"path": "b"}),
                },
            ],
            "done",
        )
        .one_tool_per_turn();
        let request = ModelRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            metadata: serde_json::Value::Null,
            cancel: agent_contracts::CancellationToken::new(),
        };
        let first = model.complete(request.clone()).await.unwrap();
        assert_eq!(first.tool_calls[0].name, "fs.read");
        let after_first = model.complete(request.clone()).await.unwrap();
        assert_eq!(after_first.content, "done");
        assert!(after_first.tool_calls.is_empty());
        let second = model.complete(request.clone()).await.unwrap();
        assert_eq!(second.tool_calls[0].name, "fs.write");
        let after_second = model.complete(request).await.unwrap();
        assert_eq!(after_second.content, "done");
    }
}

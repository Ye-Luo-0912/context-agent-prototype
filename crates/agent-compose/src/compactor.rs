//! Live 有界压缩器：同一 `ModelTransport`、无工具、源/输出都有硬上限。
//! B 折叠和 C 派生都注入这一个实现，比较的是策略而不是摘要质量。

use std::sync::Arc;

use agent_contracts::{
    AgentResult, BoundedCompactor, COMPACTION_OUTPUT_CHARS, CancellationToken, CompactionOutput,
    CompactionRequest, ModelMessage, ModelRequest, ModelTransport, bound_compaction_output,
    bound_compaction_source, tokens,
};
use async_trait::async_trait;

const COMPACTION_SYSTEM: &str = "\
You compress folded coding-agent history into a short working note. \
Keep the task goal, decisions, errors and fixes, file paths, and open loops. \
Drop repeated tool chatter, raw dumps, and greetings. \
Do not call tools. Do not invent files or results.";

/// 用当前 live 模型做有界压缩。失败或空回复回退到确定性短摘要，不拖垮回合。
pub struct ModelBackedCompactor {
    model: Arc<dyn ModelTransport>,
}

impl ModelBackedCompactor {
    pub fn new(model: Arc<dyn ModelTransport>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl BoundedCompactor for ModelBackedCompactor {
    async fn compact(&self, request: CompactionRequest) -> AgentResult<CompactionOutput> {
        let source = bound_compaction_source(&request.source);
        let fallback = fallback_text(request.folded_items, &source);
        let output = match self
            .model
            .complete(ModelRequest {
                messages: vec![
                    ModelMessage::system(COMPACTION_SYSTEM),
                    ModelMessage::user(source.clone()),
                ],
                tools: Vec::new(),
                metadata: serde_json::json!({
                    "role": "bounded-compactor",
                    "folded_items": request.folded_items,
                    "output_char_cap": COMPACTION_OUTPUT_CHARS,
                }),
                cancel: CancellationToken::new(),
            })
            .await
        {
            Ok(output) => output,
            Err(_) => {
                return Ok(CompactionOutput {
                    text: fallback,
                    input_tokens: tokens::approx_tokens(&source) as u64,
                    output_tokens: 0,
                });
            }
        };
        let mut text = bound_compaction_output(&output.content);
        if text.is_empty() {
            text = fallback;
        }
        let input_tokens = output
            .usage
            .input_tokens
            .unwrap_or_else(|| tokens::approx_tokens(&source) as u64);
        let output_tokens = output
            .usage
            .output_tokens
            .unwrap_or_else(|| tokens::approx_tokens(&text) as u64);
        Ok(CompactionOutput {
            text,
            input_tokens,
            output_tokens,
        })
    }
}

fn fallback_text(folded: usize, source: &str) -> String {
    bound_compaction_output(&format!("[compacted {folded} items] {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{AgentError, ModelCapabilities, ModelOutput, ModelUsage};

    struct RecordingModel {
        content: String,
        fail: bool,
    }

    #[async_trait]
    impl ModelTransport for RecordingModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: false,
                tool_calls: false,
                max_output_tokens: 256,
                context_window: Some(8_000),
            }
        }

        async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
            assert!(request.tools.is_empty(), "compaction must not send tools");
            if self.fail {
                return Err(AgentError::Model("boom".into()));
            }
            Ok(ModelOutput {
                content: self.content.clone(),
                tool_calls: Vec::new(),
                usage: ModelUsage {
                    input_tokens: Some(11),
                    output_tokens: Some(7),
                    ..Default::default()
                },
            })
        }
    }

    #[tokio::test]
    async fn bounds_output_and_records_usage() {
        let long = "x".repeat(COMPACTION_OUTPUT_CHARS + 80);
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: long,
            fail: false,
        }));
        let out = compactor
            .compact(CompactionRequest {
                folded_items: 4,
                source: "goal: fix auth".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.text.chars().count(), COMPACTION_OUTPUT_CHARS);
        assert_eq!(out.input_tokens, 11);
        assert_eq!(out.output_tokens, 7);
    }

    #[tokio::test]
    async fn model_failure_falls_back_without_failing_the_fold() {
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: String::new(),
            fail: true,
        }));
        let out = compactor
            .compact(CompactionRequest {
                folded_items: 2,
                source: "keep this".into(),
            })
            .await
            .unwrap();
        assert!(out.text.contains("compacted 2 items"));
        assert!(out.text.contains("keep this"));
    }
}

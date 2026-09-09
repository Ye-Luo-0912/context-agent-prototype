//! Live 有界压缩器：同一 `ModelTransport`、无工具、源/输出都有硬上限。
//! B 折叠和 C 派生都注入这一个实现，比较的是策略而不是摘要质量。

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, BoundedCompactor, COMPACTION_OUTPUT_CHARS, CancellationToken,
    CompactionOutput, CompactionRequest, ModelMessage, ModelRequest, ModelTransport,
    bound_compaction_output, bound_compaction_source, tokens,
};
use async_trait::async_trait;

const COMPACTION_SYSTEM: &str = "\
You compress folded coding-agent history into a short working note. \
Keep the task goal, decisions, errors and fixes, file paths, and open loops. \
Drop repeated tool chatter, raw dumps, and greetings. \
Do not call tools. Do not invent files or results.";

/// 用当前 live 模型做有界压缩。模型失败或空回复必须以 Err 交给引擎：
/// 显示用 fallback 前缀不能替代表达式退役源正文（W08），Rolling 的
/// 失败守卫会把折叠候选原样还回工作集，等下一次维护再消费。
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
        let output = self
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
            .await?;
        let text = bound_compaction_output(&output.content);
        if text.trim().is_empty() {
            // 调用成功但没有任何可用摘要：与失败同型（summary_unavailable），
            // 不能拿源前缀冒充折叠结果。
            return Err(AgentError::Model(
                "compaction model returned an empty summary".into(),
            ));
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
    async fn model_failure_propagates_as_summary_unavailable() {
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: String::new(),
            fail: true,
        }));
        let error = compactor
            .compact(CompactionRequest {
                folded_items: 2,
                source: "keep this".into(),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::Model(_)),
            "the model error must reach the engine so the fold is discarded: {error:?}"
        );
    }

    #[tokio::test]
    async fn empty_model_output_is_not_a_completed_summary() {
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: String::new(),
            fail: false,
        }));
        let error = compactor
            .compact(CompactionRequest {
                folded_items: 1,
                source: "unique constraint lives in the source".into(),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("empty summary"));
    }
}

/// W08 引擎级回归：模型失败后，折叠候选连同源正文中超出任何显示用
/// 前缀的尾部约束必须原样留在工作集，maintain 不得报告折叠。
#[cfg(test)]
mod engine_source_retention {
    use super::*;
    use agent_contracts::{
        ContextEngine, ContextIngress, ContextMaintenanceTrigger, ModelCapabilities, ModelOutput,
    };
    use context_baselines::RollingSummaryEngine;

    struct AlwaysFailingModel;
    #[async_trait::async_trait]
    impl ModelTransport for AlwaysFailingModel {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                context_window: Some(8_000),
                max_output_tokens: 256,
                ..Default::default()
            }
        }
        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            Err(AgentError::Model("provider down".into()))
        }
    }

    #[tokio::test]
    async fn failed_model_fold_keeps_records_beyond_any_display_prefix() {
        let engine = RollingSummaryEngine::with_config(context_baselines::RollingConfig {
            summary_threshold_tokens: 30,
            keep_most_recent_tokens: 4,
            ..Default::default()
        })
        .with_compactor(Arc::new(ModelBackedCompactor::new(Arc::new(
            AlwaysFailingModel,
        ))));
        // 唯一约束位于 512 字符显示前缀之外：旧适配器会以 Ok(前缀) 退役
        // 整条记录并丢失它。
        let constraint = "TAIL-CONSTRAINT-9f3a";
        let record = format!("{}{constraint}", "x".repeat(COMPACTION_OUTPUT_CHARS + 200));
        engine
            .ingest(ContextIngress::AssistantMessage { content: record })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert_eq!(
            report.archived, 0,
            "a failed fold must not report collapses"
        );
        let checkpoint = engine.checkpoint().await.unwrap();
        let records = checkpoint["records"].as_array().unwrap();
        assert!(
            records.iter().any(|item| item["kind"] == "AssistantMessage"
                && item["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(constraint))),
            "the record beyond any display prefix must stay in the working set: {checkpoint}"
        );
        assert!(
            !records.iter().any(|item| item["kind"] == "Summary"),
            "no summary may be minted from a failed model call"
        );
    }
}

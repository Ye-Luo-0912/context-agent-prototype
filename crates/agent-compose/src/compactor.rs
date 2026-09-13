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
                // COST-4 (D02): the output cap finally reaches the provider
                // parameter — where the transport negotiates a max-output
                // field, generation stops at the compaction bound instead of
                // the main profile's cap (truncation stays as the backstop).
                max_output_tokens: Some(COMPACTION_OUTPUT_CHARS as u32),
                cancel: CancellationToken::new(),
            })
            .await?;
        let text = bound_compaction_output(&output.content);
        if text.trim().is_empty() {
            // 调用成功但没有任何可用摘要：与失败同型（summary_unavailable），
            // 不能拿源前缀冒充折叠结果。COST-7 (R2-11)：调用本身可能已被
            // 计费——已收到的 usage 随类型化错误一起交还引擎，不再随
            // Err 一起丢失。
            return Err(AgentError::EmptyCompactionSummary {
                usage: output.usage,
            });
        }
        // CORE-4: the approximation fallback must be labelled as an
        // estimate — a runtime-derived token count never masquerades as
        // provider-reported usage.
        let usage_identity = match (output.usage.input_tokens, output.usage.output_tokens) {
            (Some(_), Some(_)) => agent_contracts::UsageIdentity::Observed,
            _ => agent_contracts::UsageIdentity::Estimated,
        };
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
            usage_identity,
            // COST-2 (E05.4)/COST-7 (R2-11): the transport's cache/attempt
            // facts travel with the output verbatim — unreported counters
            // stay `None` (never a flattened zero), and the write/miss
            // split survives to the consumer.
            cached_input_tokens: output.usage.cached_input_tokens,
            cache_write_input_tokens: output.usage.cache_write_input_tokens,
            cache_miss_input_tokens: output.usage.cache_miss_input_tokens,
            attempts: output.usage.attempts,
            retries: output.usage.retries,
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
        report_usage: bool,
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
                usage: if self.report_usage {
                    ModelUsage {
                        input_tokens: Some(11),
                        output_tokens: Some(7),
                        ..Default::default()
                    }
                } else {
                    ModelUsage::default()
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
            report_usage: true,
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
        assert_eq!(
            out.usage_identity,
            agent_contracts::UsageIdentity::Observed,
            "a full provider report is observed usage"
        );
    }

    /// CORE-4：provider 没报 usage 时运行时近似推导——数值可用，但身份
    /// 必须是 estimated，不得冒充 provider 观测。
    #[tokio::test]
    async fn a_missing_usage_report_is_labelled_estimated_not_observed() {
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: "short summary".into(),
            fail: false,
            report_usage: false,
        }));
        let out = compactor
            .compact(CompactionRequest {
                folded_items: 4,
                source: "goal: fix auth".into(),
            })
            .await
            .unwrap();
        assert!(
            out.input_tokens > 0,
            "the approximation still fills numbers"
        );
        assert_eq!(
            out.usage_identity,
            agent_contracts::UsageIdentity::Estimated,
            "derived numbers must carry the estimated identity"
        );
    }

    /// COST-4 (D02): the compactor's bounded-output contract reaches the
    /// PROVIDER parameter — the request states its own output ceiling, so
    /// generation stops at the compaction bound instead of the main
    /// profile's default (truncation afterwards is only a backstop).
    #[tokio::test]
    async fn the_output_cap_reaches_the_provider_request() {
        struct CapturingModel {
            seen_caps: std::sync::Arc<std::sync::Mutex<Vec<Option<u32>>>>,
        }
        #[async_trait::async_trait]
        impl ModelTransport for CapturingModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
            async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
                self.seen_caps
                    .lock()
                    .unwrap()
                    .push(request.max_output_tokens);
                Ok(ModelOutput {
                    content: "short summary".into(),
                    tool_calls: Vec::new(),
                    usage: ModelUsage::default(),
                })
            }
        }
        let seen_caps = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_for_model = Arc::clone(&seen_caps);
        let model: Arc<dyn ModelTransport> = Arc::new(CapturingModel {
            seen_caps: seen_for_model,
        });
        let compactor = ModelBackedCompactor::new(Arc::clone(&model));
        let _ = compactor
            .compact(CompactionRequest {
                folded_items: 2,
                source: "keep this".into(),
            })
            .await
            .unwrap();
        let caps = seen_caps.lock().unwrap().clone();
        assert_eq!(
            caps,
            vec![Some(COMPACTION_OUTPUT_CHARS as u32)],
            "the request must state the compaction output bound"
        );
    }

    #[tokio::test]
    async fn model_failure_propagates_as_summary_unavailable() {
        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: String::new(),
            fail: true,
            report_usage: true,
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
            report_usage: true,
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

    /// COST-7 (R2-11): an empty summary still discards the fold, but the
    /// usage the provider already reported for the billed call travels back
    /// on the typed error — it must not vanish with the refused result.
    #[tokio::test]
    async fn an_empty_summary_keeps_the_reported_usage_evidence() {
        struct EmptySummaryWithUsageModel;
        #[async_trait]
        impl ModelTransport for EmptySummaryWithUsageModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
            async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
                Ok(ModelOutput {
                    content: "   ".into(),
                    tool_calls: Vec::new(),
                    usage: ModelUsage {
                        input_tokens: Some(140),
                        output_tokens: Some(0),
                        cached_input_tokens: Some(90),
                        attempts: 1,
                        retries: 0,
                        ..Default::default()
                    },
                })
            }
        }
        let compactor = ModelBackedCompactor::new(Arc::new(EmptySummaryWithUsageModel));
        let error = compactor
            .compact(CompactionRequest {
                folded_items: 1,
                source: "unique constraint lives in the source".into(),
            })
            .await
            .unwrap_err();
        let usage = error
            .reported_usage()
            .expect("an empty-summary error carries the billed call's usage");
        assert_eq!(usage.input_tokens, Some(140));
        assert_eq!(usage.cached_input_tokens, Some(90));
    }

    /// COST-7 (R2-11): the cache read/write/miss split passes through
    /// verbatim — an unreported counter stays `None`, never a zero.
    #[tokio::test]
    async fn cache_buckets_pass_through_without_flattening() {
        struct CacheReportingModel;
        #[async_trait]
        impl ModelTransport for CacheReportingModel {
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities::default()
            }
            async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
                Ok(ModelOutput {
                    content: "short summary".into(),
                    tool_calls: Vec::new(),
                    usage: ModelUsage {
                        input_tokens: Some(100),
                        output_tokens: Some(7),
                        cached_input_tokens: Some(80),
                        cache_write_input_tokens: Some(10),
                        cache_miss_input_tokens: Some(20),
                        attempts: 1,
                        retries: 0,
                        ..Default::default()
                    },
                })
            }
        }
        let compactor = ModelBackedCompactor::new(Arc::new(CacheReportingModel));
        let out = compactor
            .compact(CompactionRequest {
                folded_items: 1,
                source: "keep this".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.cached_input_tokens, Some(80));
        assert_eq!(out.cache_write_input_tokens, Some(10));
        assert_eq!(out.cache_miss_input_tokens, Some(20));

        let compactor = ModelBackedCompactor::new(Arc::new(RecordingModel {
            content: "short summary".into(),
            fail: false,
            report_usage: false,
        }));
        let out = compactor
            .compact(CompactionRequest {
                folded_items: 1,
                source: "keep this".into(),
            })
            .await
            .unwrap();
        assert_eq!(out.cached_input_tokens, None);
        assert_eq!(out.cache_write_input_tokens, None);
        assert_eq!(out.cache_miss_input_tokens, None);
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

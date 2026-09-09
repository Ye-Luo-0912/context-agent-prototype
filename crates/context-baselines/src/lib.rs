//! Baseline `ContextEngine` implementations for A/B/C lifecycle experiments.
//!
//! - `AppendOnlyEngine` (A): the whole conversation is retained and resent
//!   every model turn. No lifecycle maintenance.
//! - `RollingSummaryEngine` (B): append like A, but once retained history
//!   crosses a token threshold the oldest part is collapsed into a rolling
//!   summary marker.
//!
//! Both implement the same `ContextEngine` contract as `SimpleContextEngine`
//! (C), so the kernel, tools and UI are interchangeable across all three —
//! the experiment measures the *policy*, not the plumbing.

mod append;
mod rolling;
mod shared;

pub use append::AppendOnlyEngine;
pub use rolling::{RollingConfig, RollingSummaryEngine, SUMMARIZER_PRIOR_CAP};

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        BoundedCompactor, COMPACTION_SOURCE_CHARS, CompactionOutput, CompactionRequest,
        ContextEngine, ContextHints, ContextIngress, ContextKind, ContextMaintenanceTrigger,
        ContextQuery, FocusState, MaterializedContext, MaterializedItem, TaskId, ToolOutput,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn tool_output(ok: bool, model_content: &str) -> ToolOutput {
        ToolOutput {
            call_id: "call-1".into(),
            tool_name: "shell.exec".into(),
            ok,
            summary: if ok { "ok" } else { "failed" }.into(),
            model_content: model_content.into(),
            artifact_ref: None,
            metadata: json!({}),
        }
    }

    async fn snapshot_tokens(engine: &dyn ContextEngine, input: &str) -> usize {
        let snapshot: MaterializedContext = engine
            .materialize(ContextQuery {
                current_input: input.into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        snapshot.approx_tokens
    }

    async fn run_turn(engine: &dyn ContextEngine, user: &str, rounds: usize) {
        engine
            .ingest(ContextIngress::UserMessage {
                content: user.into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::UserInput)
            .await
            .unwrap();
        for round in 0..rounds {
            engine
                .ingest(ContextIngress::ToolObservation {
                    facts: None,
                    output: tool_output(true, &format!("tool round {round} output")),
                    scope_id: None,
                })
                .await
                .unwrap();
            engine
                .maintain(ContextMaintenanceTrigger::AfterTool)
                .await
                .unwrap();
        }
        engine
            .ingest(ContextIngress::AssistantMessage {
                content: "ok".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn append_only_engine_grows_unbounded() {
        let engine = AppendOnlyEngine::new();
        for turn in 0..5 {
            run_turn(&engine, &format!("turn {turn}"), 2).await;
        }
        let tokens_early = snapshot_tokens(&engine, "next").await;
        for turn in 5..20 {
            run_turn(&engine, &format!("turn {turn}"), 2).await;
        }
        let tokens_late = snapshot_tokens(&engine, "next").await;
        let diagnostics = engine.diagnostics().await.unwrap();
        // Everything is retained and active: 20 user + 20 assistant + 40 tools.
        assert_eq!(diagnostics.total_items, 80);
        assert_eq!(diagnostics.active_items, 80);
        assert_eq!(diagnostics.tombstoned_items, 0);
        assert!(
            tokens_late > tokens_early,
            "append-only history must keep growing: {tokens_early} -> {tokens_late}"
        );
    }

    #[tokio::test]
    async fn baselines_leave_current_turn_to_the_runtime_frame() {
        let engine = AppendOnlyEngine::new();
        engine
            .ingest(ContextIngress::FocusChanged {
                focus: FocusState::for_task(TaskId::new(), "hello from the user"),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::UserMessage {
                content: "hello from the user".into(),
            })
            .await
            .unwrap();
        let first = engine
            .materialize(ContextQuery {
                current_input: "hello from the user".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        assert!(
            first.selected.is_empty() && first.items.is_empty(),
            "current user input and focus belong to TurnFrame, not A/B history: {:?}",
            first
                .items
                .iter()
                .map(|item| (item.kind, item.content.clone()))
                .collect::<Vec<_>>()
        );

        engine
            .ingest(ContextIngress::AssistantMessage {
                content: "ack".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::UserMessage {
                content: "second turn".into(),
            })
            .await
            .unwrap();
        let second = engine
            .materialize(ContextQuery {
                current_input: "second turn".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        assert!(
            second
                .items
                .iter()
                .any(|item| item.kind == ContextKind::UserMessage
                    && item.content == "hello from the user"),
            "prior user messages stay in historical context"
        );
        assert!(
            !second
                .items
                .iter()
                .any(|item| item.content == "second turn"),
            "the current user message must not duplicate TurnFrame"
        );
        assert!(
            !second
                .items
                .iter()
                .any(|item| item.kind == ContextKind::Goal),
            "FocusChanged must not mint a Goal history record"
        );

        engine
            .ingest(ContextIngress::UserMessage {
                content: "hello from the user".into(),
            })
            .await
            .unwrap();
        let repeated = engine
            .materialize(ContextQuery {
                current_input: "hello from the user".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let prior_hellos = repeated
            .items
            .iter()
            .filter(|item| {
                item.kind == ContextKind::UserMessage && item.content == "hello from the user"
            })
            .count();
        assert_eq!(
            prior_hellos, 1,
            "identical prior-turn text stays; only the current turn stamp is skipped"
        );
    }

    #[tokio::test]
    async fn rolling_summary_collapses_oldest_history() {
        // Very low thresholds so the fold triggers quickly.
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
            ..Default::default()
        });
        for turn in 0..20 {
            run_turn(&engine, &format!("turn {turn}"), 1).await;
        }
        // Collapses happened during the turn-level maintenance passes.
        let diagnostics = engine.diagnostics().await.unwrap();
        assert!(
            diagnostics.tombstoned_items > 0,
            "old history must be collapsed, dropped={}",
            diagnostics.tombstoned_items
        );
        assert!(
            diagnostics.total_items < 80,
            "collapsed records leave the working set: total={}",
            diagnostics.total_items
        );
        // A summary marker item exists.
        let items = engine.inspect(usize::MAX).await.unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.kind == agent_contracts::ContextKind::Summary),
            "collapse must leave a summary marker"
        );
        let tokens = snapshot_tokens(&engine, "next").await;
        assert!(
            tokens <= 60 + 400,
            "rolling summary must bound the snapshot, got {tokens}"
        );
    }

    /// 把折叠次数和 digest 写进标记，证明压缩器看到了被折叠正文。
    struct EchoCompactor;

    #[async_trait::async_trait]
    impl BoundedCompactor for EchoCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            Ok(CompactionOutput {
                text: format!(
                    "[rolled up {} earlier messages; digest {}]",
                    request.folded_items, request.source
                ),
                input_tokens: 4,
                output_tokens: 2,
            })
        }
    }

    #[tokio::test]
    async fn injected_summarizer_replaces_the_placeholder_marker() {
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
            ..Default::default()
        })
        .with_compactor(Arc::new(EchoCompactor));
        for turn in 0..20 {
            run_turn(&engine, &format!("turn {turn}"), 1).await;
        }
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let summary = materialized
            .items
            .iter()
            .find(|item| item.kind == agent_contracts::ContextKind::Summary)
            .expect("collapse must leave a summary marker");
        assert!(
            summary.content.starts_with("[rolled up "),
            "the injected summarizer must produce the marker, got: {}",
            summary.content
        );
        assert!(
            summary.content.contains("digest "),
            "the marker must reflect the folded content: {}",
            summary.content
        );
        assert!(
            materialized.diagnostics.compaction_input_tokens >= 4,
            "compactor usage must accumulate on diagnostics, got in={}",
            materialized.diagnostics.compaction_input_tokens
        );
        assert!(
            materialized.diagnostics.compaction_output_tokens >= 2,
            "compactor usage must accumulate on diagnostics, got out={}",
            materialized.diagnostics.compaction_output_tokens
        );
    }

    struct EmptyCompactor;

    #[async_trait::async_trait]
    impl BoundedCompactor for EmptyCompactor {
        async fn compact(
            &self,
            _request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            Ok(CompactionOutput {
                text: String::new(),
                input_tokens: 1,
                output_tokens: 0,
            })
        }
    }

    #[tokio::test]
    async fn empty_compactor_output_falls_back_to_a_bounded_marker() {
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
            ..Default::default()
        })
        .with_compactor(Arc::new(EmptyCompactor));
        for turn in 0..20 {
            run_turn(&engine, &format!("turn {turn}"), 1).await;
        }
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let summary = materialized
            .items
            .iter()
            .find(|item| item.kind == agent_contracts::ContextKind::Summary)
            .expect("collapse must leave a summary marker");
        assert!(
            summary.content.contains("Earlier context:"),
            "empty compact output must fall back, got: {}",
            summary.content
        );
    }

    /// ROLLING-PRIOR：追加一批记录并触发一次维护折叠。
    async fn feed_records(engine: &RollingSummaryEngine, prefix: &str, count: usize) {
        for index in 0..count {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("{prefix} {index}"),
                })
                .await
                .unwrap();
        }
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }

    async fn summary_item(engine: &RollingSummaryEngine) -> MaterializedItem {
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        materialized
            .items
            .into_iter()
            .find(|item| item.kind == ContextKind::Summary)
            .expect("collapse must leave a summary marker")
    }

    #[tokio::test]
    async fn later_folds_merge_the_prior_summary_into_the_next_input() {
        // ROLLING-PRIOR：第二、三次折叠的压缩输入必须包含旧摘要，第一轮
        // 独有事实不能在后续折叠中无声消失。EchoCompactor 回显输入，断言
        // 只依赖输入连续性，不依赖摘要质量。
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 30,
            keep_most_recent_tokens: 4,
            ..Default::default()
        })
        .with_compactor(Arc::new(EchoCompactor));

        engine
            .ingest(ContextIngress::AssistantMessage {
                content: "R1_HEAD first-round-only fact".into(),
            })
            .await
            .unwrap();
        feed_records(&engine, "round one record", 6).await;
        let first = summary_item(&engine).await;
        assert!(
            first.content.contains("R1_HEAD"),
            "the first fold must carry the round-one fact: {}",
            first.content
        );
        assert!(
            !first
                .source
                .as_deref()
                .unwrap_or("")
                .contains("prior summary"),
            "the first fold has no prior summary to merge: {:?}",
            first.source
        );

        feed_records(&engine, "round two record", 8).await;
        let second = summary_item(&engine).await;
        assert!(
            second.content.contains("R1_HEAD"),
            "the second fold must merge the prior summary: {}",
            second.content
        );
        assert!(
            second
                .source
                .as_deref()
                .unwrap_or("")
                .contains("prior summary"),
            "source coverage must record the merge: {:?}",
            second.source
        );

        feed_records(&engine, "round three record", 8).await;
        let third = summary_item(&engine).await;
        assert!(
            third.content.contains("R1_HEAD"),
            "the third fold must still reach the first-round fact: {}",
            third.content
        );
        assert!(
            third
                .source
                .as_deref()
                .unwrap_or("")
                .contains("prior summary"),
            "source coverage must record the merge: {:?}",
            third.source
        );
    }

    /// R08：压缩器输入容量装不下的记录绝不带「覆盖」声明移出——它们留在
    /// working set 作为可恢复残余，下一轮还能再折叠；覆盖声明只数实际
    /// 进入输入的记录，绝不宣称读过未消费的尾部。旧行为把整批 fold
    /// 候选移出却只给压缩器 2,000 字符，未读尾部悄悄退出工作集。
    #[tokio::test]
    async fn records_beyond_the_compactor_input_capacity_stay_in_the_working_set() {
        // 压缩器输入上限是 COMPACTION_SOURCE_CHARS；构造两条远大于上限的
        // 记录。旧代码两条都折叠（声称覆盖 2 条），但压缩器只看到前缀；
        // 新代码每次切分消费一个前缀、残余尾部按同 id 留在工作集直到被
        // 下轮完整消费——覆盖声明永远只含压缩器真正读到的内容。
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 1,
            keep_most_recent_tokens: 0,
            ..Default::default()
        })
        .with_compactor(Arc::new(EchoCompactor));
        let big = |tag: &str| format!("{tag}_") + &"x".repeat(COMPACTION_SOURCE_CHARS + 4000);
        engine
            .ingest(ContextIngress::AssistantMessage {
                content: big("old_huge"),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::AssistantMessage {
                content: big("new_huge"),
            })
            .await
            .unwrap();
        // 超限记录无法整条消费：维护必须产生「部分折叠」过渡，而非把
        // 未读内容整体移出让摘要谎称覆盖。两条记录各被切分 → 至少两条
        // partial 过渡（fold 逐条进行，每条超限记录一次切分）。
        let report = engine
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await
            .unwrap();
        let partial_transitions: Vec<_> = report
            .transitions
            .iter()
            .filter(|t| t.reason.contains("partially collapsed"))
            .collect();
        assert!(
            partial_transitions.len() >= 2,
            "each oversized record must fold partially with its residual kept (R08): {:?}",
            report.transitions
        );

        // 摘要或残余中至少保留一条超限记录的前缀：任何记录都不能
        // 「未读也覆盖」地一次性消失（第一条进过压缩输入，必然可见）。
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 1_000_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let summaries: Vec<String> = materialized
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::Summary)
            .map(|item| item.content.clone())
            .collect();
        let residuals: Vec<String> = materialized
            .items
            .iter()
            .filter(|item| item.kind != ContextKind::Summary)
            .map(|item| item.content.clone())
            .collect();
        let all = summaries.join("\n") + &residuals.join("\n");
        assert!(
            all.contains("old_huge_"),
            "the first over-cap record's consumed prefix must remain reachable: (summaries={summaries:?}, residuals={residuals:?})"
        );
    }

    /// R08：部分超限时消费能完整进入输入的记录，其余按容量切分前缀；
    /// 汇总文本只含实际被压缩器读到的部分。
    #[tokio::test]
    async fn fold_consumes_only_records_that_fit_the_input_capacity() {
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 1,
            keep_most_recent_tokens: 0,
            ..Default::default()
        })
        .with_compactor(Arc::new(EchoCompactor));
        // 一条小记录 + 一条超限大记录：小记录折叠，大记录保留。
        engine
            .ingest(ContextIngress::AssistantMessage {
                content: "small_kept_marker".into(),
            })
            .await
            .unwrap();
        let big = "huge_tail_".to_owned() + &"y".repeat(COMPACTION_SOURCE_CHARS + 4000);
        engine
            .ingest(ContextIngress::AssistantMessage { content: big })
            .await
            .unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::Checkpoint)
            .await
            .unwrap();
        assert!(
            report
                .transitions
                .iter()
                .any(|t| t.reason.contains("partially collapsed")),
            "the oversized record must fold partially while its residual stays (R08): {:?}",
            report.transitions
        );

        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 1_000_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let summaries: Vec<String> = materialized
            .items
            .iter()
            .filter(|item| item.kind == ContextKind::Summary)
            .map(|item| item.content.clone())
            .collect();
        assert!(
            summaries
                .iter()
                .any(|content| content.contains("small_kept_marker")),
            "a record that fits the input capacity is consumed into the summary: {summaries:?}"
        );
    }

    struct FailingCompactor;

    #[async_trait::async_trait]
    impl BoundedCompactor for FailingCompactor {
        async fn compact(
            &self,
            _request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            Err(agent_contracts::AgentError::Internal(
                "compactor down".into(),
            ))
        }
    }

    #[tokio::test]
    async fn failed_compaction_preserves_the_pre_fold_state() {
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 30,
            keep_most_recent_tokens: 4,
            ..Default::default()
        })
        .with_compactor(Arc::new(FailingCompactor));
        for index in 0..10 {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("history record {index}"),
                })
                .await
                .unwrap();
        }
        let before = engine.diagnostics().await.unwrap();
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert!(
            report.transitions.is_empty() && report.archived == 0,
            "a failed fold must not report collapses (archived={})",
            report.archived
        );
        let after = engine.diagnostics().await.unwrap();
        assert_eq!(
            after.total_items, before.total_items,
            "a failed fold must return the records to the working set"
        );
        assert_eq!(
            after.tombstoned_items, before.tombstoned_items,
            "a failed fold must roll back the collapsed count"
        );
        assert_eq!(
            after.approx_active_tokens, before.approx_active_tokens,
            "a failed fold must not change the retained volume"
        );
        let items = engine.inspect(usize::MAX).await.unwrap();
        assert!(
            items.iter().all(|item| item.kind != ContextKind::Summary),
            "a failed fold must not mint a summary marker"
        );
    }

    /// W04：压缩器调用计数器——一次维护串行调用的次数必须有预算上限。
    struct CountingCompactor(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl BoundedCompactor for CountingCompactor {
        async fn compact(
            &self,
            request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(CompactionOutput {
                text: format!(
                    "[summary of {}]",
                    request.source.chars().take(24).collect::<String>()
                ),
                input_tokens: 10,
                output_tokens: 4,
            })
        }
    }

    #[tokio::test]
    async fn maintain_bounds_serial_compactor_calls_and_reports_the_deferred_rest() {
        let counting = Arc::new(CountingCompactor(std::sync::atomic::AtomicUsize::new(0)));
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 30,
            keep_most_recent_tokens: 4,
            max_compactor_calls_per_maintain: 2,
        })
        .with_compactor(Arc::clone(&counting) as Arc<dyn BoundedCompactor>);
        for index in 0..30 {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("history record {index}: {}", "x".repeat(500)),
                })
                .await
                .unwrap();
        }
        let first = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert_eq!(
            counting.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one maintain must not exceed the configured call budget"
        );
        assert!(
            first.deferred_folds > 0,
            "a budget-limited pass must honestly report the deferred remainder"
        );
        // 延期不是丢失：下一次维护继续消费同一批候选。
        let second = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        assert_eq!(
            counting.0.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "the next maintain continues the deferred folds"
        );
        assert!(second.deferred_folds > 0 || second.archived > 0);
        // 持续维护最终收敛：阈值满足后不再有延期。
        let mut settled = second;
        for _ in 0..20 {
            if settled.deferred_folds == 0 {
                break;
            }
            settled = engine
                .maintain(ContextMaintenanceTrigger::AfterModel)
                .await
                .unwrap();
        }
        assert_eq!(
            settled.deferred_folds, 0,
            "repeated bounded maintains must eventually satisfy the threshold"
        );
    }

    struct GatedCompactor {
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl BoundedCompactor for GatedCompactor {
        async fn compact(
            &self,
            _request: CompactionRequest,
        ) -> agent_contracts::AgentResult<CompactionOutput> {
            self.entered.notify_one();
            // 永不返回：maintain future 只能在该 await 点被丢弃。
            std::future::pending::<()>().await;
            unreachable!("the gated compactor never returns")
        }
    }

    #[tokio::test]
    async fn dropped_maintain_future_returns_fold_candidates_to_the_working_set() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 30,
            keep_most_recent_tokens: 4,
            ..Default::default()
        })
        .with_compactor(Arc::new(GatedCompactor {
            entered: Arc::clone(&entered),
        }));
        for index in 0..10 {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("history record {index}"),
                })
                .await
                .unwrap();
        }
        let before = engine.diagnostics().await.unwrap();

        {
            let maintain = engine.maintain(ContextMaintenanceTrigger::AfterModel);
            tokio::select! {
                _ = maintain => panic!("maintain must block while the compactor is gated"),
                _ = entered.notified() => {}
            }
        }

        let after = engine.diagnostics().await.unwrap();
        assert_eq!(
            after.total_items, before.total_items,
            "cancellation must return the removed records"
        );
        assert_eq!(
            after.tombstoned_items, before.tombstoned_items,
            "cancellation must roll back the collapsed count"
        );
        assert_eq!(
            after.approx_active_tokens, before.approx_active_tokens,
            "cancellation must not change the retained volume"
        );

        // 归还必须保持原始顺序与内容（守卫按原顺序插回队首）。
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        let bodies: Vec<&str> = materialized
            .items
            .iter()
            .map(|item| item.content.as_str())
            .collect();
        let expected: Vec<String> = (0..10)
            .map(|index| format!("history record {index}"))
            .collect();
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        assert_eq!(
            bodies, expected,
            "restored records must keep their original order and content"
        );
        assert!(
            materialized
                .items
                .iter()
                .all(|item| item.kind != ContextKind::Summary),
            "a cancelled fold must not mint a summary marker"
        );
    }

    #[tokio::test]
    async fn rolling_tracks_focus_for_the_restore_authority_check() {
        // ROLLING-FOCUS：默认 profile（Rolling）的活动任务检查点恢复依赖
        // diagnostics.focus_task_id 与 runtime 任务对齐（kernel 恢复侧的
        // focus 权威校验）。
        let engine = RollingSummaryEngine::new();
        let task_a = TaskId::new();
        engine
            .ingest(ContextIngress::FocusChanged {
                focus: FocusState::for_task(task_a, "refactor auth"),
            })
            .await
            .unwrap();
        let diagnostics = engine.diagnostics().await.unwrap();
        assert_eq!(diagnostics.focus_task_id, Some(task_a));
        assert_eq!(
            diagnostics.focus_generation, 1,
            "each focus change bumps the generation"
        );
        let materialized = engine
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            materialized.focus.as_ref().map(|focus| focus.task_id),
            Some(task_a)
        );

        // 检查点往返保留聚焦身份：恢复校验在 restore 之后读 diagnostics。
        let checkpoint = engine.checkpoint().await.unwrap();
        let fresh = RollingSummaryEngine::new();
        fresh.restore(checkpoint).await.unwrap();
        assert_eq!(
            fresh.diagnostics().await.unwrap().focus_task_id,
            Some(task_a)
        );

        // 无关任务的完成不清除聚焦。
        engine
            .ingest(ContextIngress::TaskCompleted {
                task_id: Some(TaskId::new()),
                summary: "unrelated".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            engine.diagnostics().await.unwrap().focus_task_id,
            Some(task_a),
            "an unrelated completion must not clear the focus"
        );

        // 聚焦任务完成 → 清除。
        engine
            .ingest(ContextIngress::TaskCompleted {
                task_id: Some(task_a),
                summary: "done".into(),
            })
            .await
            .unwrap();
        assert_eq!(engine.diagnostics().await.unwrap().focus_task_id, None);

        // 重新聚焦后挂起 → 清除。
        let task_b = TaskId::new();
        engine
            .ingest(ContextIngress::FocusChanged {
                focus: FocusState::for_task(task_b, "write docs"),
            })
            .await
            .unwrap();
        assert_eq!(
            engine.diagnostics().await.unwrap().focus_task_id,
            Some(task_b)
        );
        engine.ingest(ContextIngress::FocusCleared).await.unwrap();
        assert_eq!(engine.diagnostics().await.unwrap().focus_task_id, None);
    }

    #[tokio::test]
    async fn checkpoint_restore_roundtrip_for_both_baselines() {
        // Append-only roundtrip.
        let engine = AppendOnlyEngine::new();
        run_turn(&engine, "hello", 1).await;
        let data = engine.checkpoint().await.unwrap();
        let tokens_before = snapshot_tokens(&engine, "again").await;
        let fresh = AppendOnlyEngine::new();
        fresh.restore(data).await.unwrap();
        let tokens_after = snapshot_tokens(&fresh, "again").await;
        assert_eq!(tokens_before, tokens_after);
        assert_eq!(
            engine.diagnostics().await.unwrap().total_items,
            fresh.diagnostics().await.unwrap().total_items
        );

        // Rolling roundtrip (with a collapse already performed).
        let rolling = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
            ..Default::default()
        });
        for turn in 0..10 {
            run_turn(&rolling, &format!("turn {turn}"), 1).await;
        }
        let data = rolling.checkpoint().await.unwrap();
        let diagnostics_before = rolling.diagnostics().await.unwrap();
        let fresh_rolling = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
            ..Default::default()
        });
        fresh_rolling.restore(data).await.unwrap();
        let diagnostics_after = fresh_rolling.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics_before.total_items,
            diagnostics_after.total_items
        );
        assert_eq!(
            diagnostics_before.tombstoned_items,
            diagnostics_after.tombstoned_items
        );
    }

    async fn seed_history(engine: &dyn ContextEngine, count: usize) {
        for index in 0..count {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("history record {index}"),
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn baselines_honor_max_selected_items_and_budget_caps() {
        let append = AppendOnlyEngine::new();
        seed_history(&append, 257).await;
        let materialized = append
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints {
                    max_selected_items: Some(256),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(materialized.items.len(), 256);
        assert_eq!(materialized.selected.len(), 256);
        materialized.validate_materialization().unwrap();

        // Without the item cap the baseline keeps its unbounded history:
        // the A/B/C comparison deliberately measures that cost.
        let uncapped = append
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            uncapped.items.len(),
            257,
            "baseline stays unbounded without a cap"
        );

        let rolling = RollingSummaryEngine::new();
        seed_history(&rolling, 257).await;
        let materialized = rolling
            .materialize(ContextQuery {
                current_input: "next".into(),
                budget_tokens: 100_000,
                hints: ContextHints {
                    max_selected_items: Some(256),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(materialized.selected.len(), 256);
        materialized.validate_materialization().unwrap();
    }
}

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
pub use rolling::{RollingConfig, RollingSummaryEngine};

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ContextEngine, ContextHints, ContextIngress, ContextMaintenanceTrigger, ContextQuery,
        MaterializedContext, ToolOutput,
    };
    use serde_json::json;

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
        assert_eq!(diagnostics.dropped_items, 0);
        assert!(
            tokens_late > tokens_early,
            "append-only history must keep growing: {tokens_early} -> {tokens_late}"
        );
    }

    #[tokio::test]
    async fn rolling_summary_collapses_oldest_history() {
        // Very low thresholds so the fold triggers quickly.
        let engine = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
        });
        for turn in 0..20 {
            run_turn(&engine, &format!("turn {turn}"), 1).await;
        }
        // Collapses happened during the turn-level maintenance passes.
        let diagnostics = engine.diagnostics().await.unwrap();
        assert!(
            diagnostics.dropped_items > 0,
            "old history must be collapsed, dropped={}",
            diagnostics.dropped_items
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
        });
        for turn in 0..10 {
            run_turn(&rolling, &format!("turn {turn}"), 1).await;
        }
        let data = rolling.checkpoint().await.unwrap();
        let diagnostics_before = rolling.diagnostics().await.unwrap();
        let fresh_rolling = RollingSummaryEngine::with_config(RollingConfig {
            summary_threshold_tokens: 60,
            keep_most_recent_tokens: 20,
        });
        fresh_rolling.restore(data).await.unwrap();
        let diagnostics_after = fresh_rolling.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics_before.total_items,
            diagnostics_after.total_items
        );
        assert_eq!(
            diagnostics_before.dropped_items,
            diagnostics_after.dropped_items
        );
    }
}

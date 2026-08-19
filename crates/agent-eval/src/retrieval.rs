//! Catalog + 分级访问的 M15 检索引擎基线。
//!
//! 这不是配对真模型编码门。它先把独特事实挤出 GC 缓冲（外置），再
//! 用 `context.search` 按 needle 找回，并记录搜索延迟与分级 access 戳。
//! 编码 fixture 的 `--compare-arm` 也会从事件流打印同一组 retrieval 行
//! （脚本通常不 search，那些行多为 0）。

use std::time::Instant;

use agent_contracts::{
    ContextEngine, ContextIngress, ContextMaintenanceTrigger, ContextSearchQuery, FocusState,
    TaskId, ToolOutput,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};

/// 一次仅引擎的检索测量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalReport {
    pub forgotten: u64,
    pub search_calls: u64,
    pub search_hits: u64,
    pub recovered: u64,
    pub search_ms_p50: u64,
    pub search_ms_p95: u64,
    pub access_search_hits: u64,
    pub access_inspects: u64,
    pub access_fetches: u64,
}

impl RetrievalReport {
    pub fn recall_bps(&self) -> u64 {
        if self.forgotten == 0 {
            return 0;
        }
        self.recovered.saturating_mul(10_000) / self.forgotten
    }
}

pub fn render_retrieval(report: &RetrievalReport) -> String {
    format!(
        "retrieval baseline (catalog + graded access, engine-only):\n\
         forgotten={} recovered={} recall_bps={} (10000=all found)\n\
         search: calls={} hits={} latency p50={}ms p95={}ms\n\
         access: search_hits={} inspects={} fetches={}\n",
        report.forgotten,
        report.recovered,
        report.recall_bps(),
        report.search_calls,
        report.search_hits,
        report.search_ms_p50,
        report.search_ms_p95,
        report.access_search_hits,
        report.access_inspects,
        report.access_fetches,
    )
}

/// 写入独特事实，迫使缓冲溢出外置，再按 needle 搜索。
/// 召回按 GC 报告的 `externalized_ids` 计，不按 needle 条数计。
/// ToolObservation 的检索面是 identity（stamped `path@rev`），不是 stdout；
/// 基线把 needle 挂在 `metadata.path` 上，Fetch 仍读回正文。
pub async fn run_retrieval_baseline() -> anyhow::Result<RetrievalReport> {
    let dir = tempfile::tempdir()?;
    // 配方对齐 `fetch_external_recovers_the_exact_original_content`：
    // 容量 1 的可逆缓冲 + 关闭热实体召回，避免刚外置又被下一轮 GC 拉回。
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 0,
        entity_affinity: false,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "unrelated session"),
        })
        .await?;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "keep a long-running workspace".into(),
        })
        .await?;

    let needles = [
        "unique-fact-alpha-retrieval",
        "unique-fact-bravo-retrieval",
        "unique-fact-charlie-retrieval",
        "unique-fact-delta-retrieval",
    ];
    for (index, needle) in needles.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: format!("obs-{index}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("step {index}: {needle}"),
                    artifact_ref: None,
                    metadata: serde_json::json!({
                        "path": needle,
                        "revision": format!("rev-{index}"),
                    }),
                },
                scope_id: None,
            })
            .await?;
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await?;
    let gc = engine.gc().await?;
    anyhow::ensure!(
        !gc.externalized_ids.is_empty(),
        "retrieval baseline expected buffer overflow to externalize at least one item: {gc:?}"
    );
    anyhow::ensure!(
        gc.externalized_ids.len() == gc.externalized,
        "externalized_ids must match the pass count: {gc:?}"
    );
    let forgotten = gc.externalized_ids;

    let mut search_ms = Vec::new();
    let mut search_hits = 0u64;
    let mut recovered_ids = std::collections::HashSet::new();
    for needle in needles {
        let started = Instant::now();
        // 不按 kind 过滤：命中 stamped path / identity 卡，不扫 stdout。
        let hits = engine
            .search_external(ContextSearchQuery::new(needle, 8))
            .await?;
        search_ms.push(started.elapsed().as_millis() as u64);
        search_hits += hits.len() as u64;
        for hit in hits {
            if forgotten.contains(&hit.item_id) {
                recovered_ids.insert(hit.item_id);
            }
        }
    }

    if let Some(first) = forgotten.first().copied() {
        let _ = engine.inspect_external(first).await?;
        let _ = engine.fetch_external(first).await?;
    }
    let diagnostics = engine.diagnostics().await?;
    search_ms.sort_unstable();
    Ok(RetrievalReport {
        forgotten: forgotten.len() as u64,
        search_calls: needles.len() as u64,
        search_hits,
        recovered: recovered_ids.len() as u64,
        search_ms_p50: percentile(&search_ms, 50),
        search_ms_p95: percentile(&search_ms, 95),
        access_search_hits: diagnostics.access_search_hits,
        access_inspects: diagnostics.access_inspects,
        access_fetches: diagnostics.access_fetches,
    })
}

fn percentile(sorted: &[u64], pct: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * pct as f64 / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retrieval_baseline_finds_externalized_facts() {
        let report = run_retrieval_baseline().await.expect("baseline");
        assert!(
            report.forgotten >= 1,
            "GC must forget at least one item: {report:?}"
        );
        assert!(
            report.recovered >= 1,
            "search must recover at least one forgotten item: {report:?}"
        );
        assert!(
            report.search_hits >= 1,
            "search must return descriptor hits: {report:?}"
        );
        assert!(
            report.access_search_hits >= 1,
            "search-hit stamps must be counted: {report:?}"
        );
        assert!(
            report.access_inspects >= 1 && report.access_fetches >= 1,
            "inspect/fetch stamps must be counted: {report:?}"
        );
        let rendered = render_retrieval(&report);
        assert!(rendered.contains("forgotten="));
        assert!(rendered.contains("recall_bps="));
    }
}

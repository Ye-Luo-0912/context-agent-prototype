//! Catalog + 分级访问的检索引擎基线。
//!
//! 这不是配对真模型编码门。它先把独特事实挤出 GC 缓冲（外置），再
//! 用 `context.search` 按 needle 找回，并记录搜索延迟与分级 access 戳。
//! 编码 fixture 的 `--compare-arm` 也会从事件流打印同一组 retrieval 行
//! （脚本通常不 search，那些行多为 0）。

use std::time::Instant;

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextMaintenanceTrigger, ContextSearchQuery,
    FocusState, TaskId, ToolOutput,
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
                facts: None,
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

/// 复杂检索场景（真实引擎调用链）：ingest → GC 外置 → search_external。
/// 多词 needle 的 token 全部在场但从不连续出现，用于区分"整段子串
/// 校验"与"token 覆盖校验"两代实现；单词与 identity 控制组在两代
/// 实现上都必须命中，作为回归守卫。逐行打印 `RC,` 前缀结果。
pub async fn run_retrieval_complex_baseline() -> anyhow::Result<String> {
    let dir = tempfile::tempdir()?;
    // 小缓冲 + 关闭热实体召回：让语义条目真正被挤出外置，走存储侧
    // 检索路径；Pin 条目保持驻留，语料同时覆盖两种 residency。
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 2,
        gc_reactivate_per_pass: 0,
        entity_affinity: false,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_id = TaskId::new();
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(task_id, "complex retrieval session"),
        })
        .await?;

    // 多词语义埋入（UserMessage 摘要保留正文）。
    let planted: &[&str] = &[
        "auth service token refresh raised after the load test timeout review",
        "db pool sized for burst traffic; connection breaker added",
        "retry jitter added for flaky network calls under packet loss",
        "cache eviction switched to an lru approximation under pressure",
        "secret rotation enforced nightly; lease bound to prod keys",
    ];
    for content in planted {
        engine
            .ingest(ContextIngress::UserMessage {
                content: (*content).into(),
            })
            .await?;
    }
    // 干扰语料：与任何 needle token 无重叠。
    const TOPICS: [&str; 12] = [
        "logging",
        "metrics",
        "tracing",
        "config",
        "schema",
        "migration",
        "queue",
        "worker",
        "cron",
        "tls",
        "dns",
        "proxy",
    ];
    const VERBS: [&str; 6] = [
        "enabled",
        "disabled",
        "tuned",
        "documented",
        "reviewed",
        "deferred",
    ];
    for i in 0..15 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!(
                    "{} setup {} for component{}",
                    TOPICS[i % TOPICS.len()],
                    VERBS[(i / TOPICS.len()) % VERBS.len()],
                    i
                ),
            })
            .await?;
    }
    // identity 控制：ToolObservation 检索面是 stamped path@rev。
    let identity_paths = [
        "unique-fact-alpha-complex",
        "unique-fact-bravo-complex",
        "unique-fact-charlie-complex",
    ];
    for (index, path) in identity_paths.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: format!("cobs-{index}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("step {index}: {path}"),
                    artifact_ref: None,
                    metadata: serde_json::json!({
                        "path": path,
                        "revision": format!("crev-{index}"),
                    }),
                },
                scope_id: None,
            })
            .await?;
    }
    // kind 过滤用例的载体：驻留 Constraint。
    engine
        .ingest(ContextIngress::Pin {
            content: "deploy rollback shortened to a one-hour window by policy".into(),
            kind: ContextKind::Constraint,
        })
        .await?;

    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await?;
    let gc = engine.gc().await?;
    anyhow::ensure!(
        !gc.externalized_ids.is_empty(),
        "complex retrieval expected buffer overflow: {gc:?}"
    );
    let forgotten = gc.externalized_ids.len();

    // (name, needle, 相关文档内容 marker, kind 过滤)。marker 取自埋入
    // 文案，用于在返回投影中认定相关命中。
    let cases: &[(&str, &str, &str, Option<ContextKind>)] = &[
        ("auth_timeout", "auth timeout", "load test timeout", None),
        (
            "connection_pool",
            "connection pool",
            "connection breaker",
            None,
        ),
        (
            "retry_flaky_network",
            "retry flaky network",
            "flaky network calls",
            None,
        ),
        (
            "cache_eviction_lru",
            "cache eviction lru",
            "lru approximation",
            None,
        ),
        (
            "secret_rotation_lease",
            "secret rotation lease",
            "lease bound to prod keys",
            None,
        ),
        (
            "rollback_window_constraint",
            "rollback window",
            "one-hour window",
            Some(ContextKind::Constraint),
        ),
        ("single_token_jitter", "jitter", "retry jitter added", None),
        ("single_token_nightly", "nightly", "enforced nightly", None),
        (
            "identity_alpha",
            "unique-fact-alpha-complex",
            "unique-fact-alpha-complex",
            None,
        ),
        (
            "identity_bravo",
            "unique-fact-bravo-complex",
            "unique-fact-bravo-complex",
            None,
        ),
        ("absent_terms", "zzqq noth1ng", "", None),
    ];

    let mut out = String::new();
    out.push_str(&format!(
        "RC,meta,forgotten={forgotten},cases={}\n",
        cases.len()
    ));
    out.push_str(
        "RC,row,name,category,hits,total_relevant,first_rank,recovered_from_gc,us_per_query\n",
    );
    let mut cat_stats: Vec<(&str, usize, usize)> = Vec::new();
    for (name, needle, marker, kind) in cases {
        let query = ContextSearchQuery {
            kind: *kind,
            ..ContextSearchQuery::new(*needle, 8)
        };
        let started = Instant::now();
        let hits = engine.search_external(query.clone()).await?;
        let us = started.elapsed().as_micros() as u64;
        let again = engine.search_external(query).await?;
        let ids: Vec<_> = hits.iter().map(|h| h.item_id).collect();
        let again_ids: Vec<_> = again.iter().map(|h| h.item_id).collect();
        anyhow::ensure!(
            ids == again_ids,
            "complex retrieval case {name} must be deterministic"
        );

        let relevant =
            |h: &agent_contracts::ExternalizedContext| h.context_ref.summary.contains(marker);
        let total_relevant = hits.iter().filter(|h| relevant(h)).count();
        let first_rank = hits.iter().position(&relevant).map(|p| p + 1);
        let recovered = hits
            .iter()
            .filter(|h| relevant(h) && gc.externalized_ids.contains(&h.item_id))
            .count();
        let category = if kind.is_some() {
            "kind_filtered"
        } else if name.starts_with("identity") {
            "identity_control"
        } else if name.starts_with("single_token") {
            "single_word_control"
        } else if *name == "absent_terms" {
            "control"
        } else {
            "multi_word"
        };
        // 回归守卫：两代实现都必须成立的控制组断言。
        if category == "control" {
            anyhow::ensure!(
                total_relevant == 0 && hits.is_empty(),
                "absent terms must stay empty"
            );
        }
        if category == "single_word_control" || category == "identity_control" {
            anyhow::ensure!(
                total_relevant >= 1,
                "control case {name} must keep hitting on both implementations"
            );
        }
        let rank = first_rank.map_or("-".into(), |r| r.to_string());
        out.push_str(&format!(
            "RC,row,{name},{category},{},{total_relevant},{rank},{recovered},{us}\n",
            hits.len()
        ));
        let hit_case = usize::from(!hits.is_empty());
        match cat_stats.iter_mut().find(|(c, _, _)| *c == category) {
            Some(row) => {
                row.1 += 1;
                row.2 += hit_case;
            }
            None => cat_stats.push((category, 1, hit_case)),
        }
    }

    // 分类汇总（多词类不设断言——它正是两代实现的差异所在）。
    for (category, n, hit_cases) in cat_stats {
        out.push_str(&format!(
            "RC,summary,{category},cases={n},hit_cases={hit_cases}/{n}\n"
        ));
    }
    Ok(out)
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

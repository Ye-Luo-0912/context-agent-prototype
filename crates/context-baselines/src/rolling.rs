//! Baseline B: append + periodic summary at a threshold.
//!
//! Everything is appended like baseline A, but when the retained history
//! crosses `summary_threshold_tokens` the oldest records — those strictly
//! older than the newest `keep_most_recent_tokens` verbatim window — are
//! folded into a single rolling summary marker. This models the classic
//! "summarize when the window fills" baseline. The marker defaults to a
//! bounded placeholder; inject a [`BoundedCompactor`] so live B uses the
//! same model-backed operator as C's task and episode distillation. CI keeps a
//! scripted digest. Fold work is taken under the mutex, then the
//! compactor runs without holding it.

use std::sync::{Arc, Mutex as StdMutex};

use agent_contracts::{
    AgentError, AgentResult, AttentionState, BoundedCompactor, COMPACTION_SOURCE_CHARS,
    CompactionOutput, CompactionReason, CompactionRequest, ContextCompaction, ContextDiagnostics,
    ContextEngine, ContextIngress, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextScope, ContextSelection,
    ContextStateTransition, MaterializedContext, ScopeId, ScopeKind, ScoreBreakdown,
    bound_compaction_output, bound_compaction_source,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::shared::{
    Record, active_diagnostics, approx_tokens, materialized_items, records_for_ingress,
};

/// Configuration for the rolling-summary baseline.
#[derive(Debug, Clone)]
pub struct RollingConfig {
    /// Collapse oldest history while total retained tokens exceed this.
    pub summary_threshold_tokens: usize,
    /// The newest records covering up to this many tokens stay verbatim;
    /// anything older is a fold candidate.
    pub keep_most_recent_tokens: usize,
}

impl Default for RollingConfig {
    fn default() -> Self {
        Self {
            summary_threshold_tokens: 9_000,
            keep_most_recent_tokens: 8_000,
        }
    }
}

/// 兼容旧名：折叠正文交给压缩器之前的字符上限。
pub const SUMMARIZER_PRIOR_CAP: usize = COMPACTION_SOURCE_CHARS;

#[derive(Debug, Default, Serialize, Deserialize)]
struct RollingState {
    /// Oldest first.
    records: Vec<Record>,
    /// Running collapse marker (None until the first collapse).
    summary: Option<Record>,
    /// Total number of records folded into the marker.
    collapsed: usize,
    turn: u64,
    #[serde(default)]
    materialization_revision: u64,
    #[serde(default)]
    compaction_input_tokens: u64,
    #[serde(default)]
    compaction_output_tokens: u64,
}

impl RollingState {
    fn total_tokens(&self) -> usize {
        let records: usize = self
            .records
            .iter()
            .map(|record| approx_tokens(&record.content))
            .sum();
        let summary = self
            .summary
            .as_ref()
            .map_or(0, |record| approx_tokens(&record.content));
        records + summary
    }

    fn diagnostics(&self) -> ContextDiagnostics {
        let mut diagnostics =
            active_diagnostics(&self.records, self.summary.as_ref(), self.collapsed);
        diagnostics.compaction_input_tokens = self.compaction_input_tokens;
        diagnostics.compaction_output_tokens = self.compaction_output_tokens;
        diagnostics
    }
}

/// Baseline B context engine: append, then collapse the oldest history into a
/// rolling summary once a token threshold is crossed.
pub struct RollingSummaryEngine {
    state: StdMutex<RollingState>,
    config: RollingConfig,
    /// 注入后每次折叠走有界压缩器；缺省仍用固定占位标记。
    compactor: Option<Arc<dyn BoundedCompactor>>,
}

impl RollingSummaryEngine {
    pub fn new() -> Self {
        Self::with_config(RollingConfig::default())
    }

    pub fn with_config(config: RollingConfig) -> Self {
        Self {
            state: StdMutex::new(RollingState::default()),
            config,
            compactor: None,
        }
    }

    /// B 与 C 共用的有界压缩器。脚本化实现留给 CI；live 注入模型实现。
    pub fn with_compactor(mut self, compactor: Arc<dyn BoundedCompactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    fn take_fold_job(&self) -> Option<FoldJob> {
        let mut state = self.state.lock().expect("rolling state poisoned");
        if state.total_tokens() <= self.config.summary_threshold_tokens {
            return None;
        }
        let mut kept_tokens = 0usize;
        let mut fold_candidates = 0usize;
        for record in state.records.iter().rev() {
            if kept_tokens >= self.config.keep_most_recent_tokens {
                fold_candidates += 1;
            }
            kept_tokens += approx_tokens(&record.content);
        }
        if fold_candidates == 0 {
            return None;
        }
        let mut prior = String::new();
        let mut transitions = Vec::new();
        for _ in 0..fold_candidates {
            let record = state.records.remove(0);
            if prior.chars().count() < SUMMARIZER_PRIOR_CAP {
                prior.push_str(&record.content);
                prior.push('\n');
            }
            state.collapsed += 1;
            transitions.push(ContextStateTransition {
                item_id: record.id,
                kind: record.kind,
                scope: record.scope,
                from: AttentionState::Active,
                to: AttentionState::Archived,
                turn: state.turn,
                reason: "collapsed into rolling summary (baseline B)".into(),
            });
        }
        let summary_id = state
            .summary
            .as_ref()
            .map(|summary| summary.id)
            .unwrap_or_default();
        Some(FoldJob {
            prior: bound_compaction_source(&prior),
            collapsed: state.collapsed,
            summary_id,
            transitions,
        })
    }

    async fn compact_fold(&self, job: &FoldJob) -> CompactionOutput {
        let fallback = fallback_marker(job.collapsed, &job.prior);
        let Some(compactor) = &self.compactor else {
            return CompactionOutput {
                text: fallback,
                ..CompactionOutput::default()
            };
        };
        match compactor
            .compact(CompactionRequest {
                folded_items: job.collapsed,
                source: job.prior.clone(),
            })
            .await
        {
            Ok(mut output) => {
                output.text = bound_compaction_output(&output.text);
                if output.text.is_empty() {
                    output.text = fallback;
                }
                output
            }
            Err(_) => CompactionOutput {
                text: fallback,
                ..CompactionOutput::default()
            },
        }
    }
}

struct FoldJob {
    prior: String,
    collapsed: usize,
    summary_id: agent_contracts::ContextItemId,
    transitions: Vec<ContextStateTransition>,
}

fn fallback_marker(collapsed: usize, prior: &str) -> String {
    bound_compaction_output(&format!(
        "Earlier context: {collapsed} prior messages collapsed. {prior}"
    ))
}

impl Default for RollingSummaryEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContextEngine for RollingSummaryEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let mut state = self.state.lock().expect("rolling state poisoned");
        if matches!(ingress, ContextIngress::UserMessage { .. }) {
            state.turn += 1;
        }
        let records = records_for_ingress(&ingress, state.turn);
        state.records.extend(records);
        Ok(())
    }

    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        // 折叠从锁里取出源文本后必须放开锁再调压缩器：模型调用不能占着
        // StdMutex。记录已在取 job 时移出 working set；压缩失败则写回
        // 有界占位，B 仍然完成折叠而不是卡死回合。
        let mut transitions: Vec<ContextStateTransition> = Vec::new();
        let mut pass_in = 0u64;
        let mut pass_out = 0u64;
        let mut compactions = Vec::new();
        while let Some(job) = self.take_fold_job() {
            let compacted = self.compact_fold(&job).await;
            pass_in = pass_in.saturating_add(compacted.input_tokens);
            pass_out = pass_out.saturating_add(compacted.output_tokens);
            if compacted.input_tokens > 0 || compacted.output_tokens > 0 {
                compactions.push(ContextCompaction {
                    reason: CompactionReason::RollingFold,
                    input_tokens: compacted.input_tokens,
                    output_tokens: compacted.output_tokens,
                    source_items: job.collapsed,
                });
            }
            {
                let mut state = self.state.lock().expect("rolling state poisoned");
                state.compaction_input_tokens = state
                    .compaction_input_tokens
                    .saturating_add(compacted.input_tokens);
                state.compaction_output_tokens = state
                    .compaction_output_tokens
                    .saturating_add(compacted.output_tokens);
                state.summary = Some(Record {
                    id: job.summary_id,
                    kind: ContextKind::Summary,
                    scope: ContextScope::Task,
                    content: compacted.text,
                    created_turn: 0,
                    source: Some("rolling summary".into()),
                });
            }
            transitions.extend(job.transitions);
        }

        let state = self.state.lock().expect("rolling state poisoned");
        Ok(ContextMaintenanceReport {
            archived: transitions.len(),
            turn: state.turn,
            transitions,
            diagnostics: state.diagnostics(),
            compaction_input_tokens: pass_in,
            compaction_output_tokens: pass_out,
            compactions,
            ..ContextMaintenanceReport::default()
        })
    }

    // The baseline retains no scope tree: scope ids are accepted so the
    // runtime's execution-frame protocol works against any engine, and
    // closing is a no-op because nothing is ever scoped.
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }

    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }

    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        let mut state = self.state.lock().expect("rolling state poisoned");
        state.materialization_revision =
            state
                .materialization_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::Internal("context materialization id is exhausted".into())
                })?;
        let items = materialized_items(&state.records, state.summary.as_ref());
        let approx_tokens_total: usize = items
            .iter()
            .map(|item| approx_tokens(&item.content))
            .sum::<usize>();
        let mut selected: Vec<ContextSelection> = Vec::new();
        if let Some(summary) = &state.summary {
            selected.push(ContextSelection {
                item_id: summary.id,
                score: 1.0,
                approx_tokens: approx_tokens(&summary.content),
                reason: "rolling summary (baseline B)".into(),
                breakdown: ScoreBreakdown::default(),
            });
        }
        selected.extend(state.records.iter().map(|record| ContextSelection {
            item_id: record.id,
            score: 1.0,
            approx_tokens: approx_tokens(&record.content),
            reason: "append + rolling summary baseline: retained history".into(),
            breakdown: ScoreBreakdown::default(),
        }));
        Ok(MaterializedContext {
            materialization_id: state.materialization_revision,
            focus: None,
            task: None,
            items,
            external: agent_contracts::ContextMapView::default(),
            selected,
            approx_tokens: approx_tokens_total,
            diagnostics: state.diagnostics(),
        })
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().expect("rolling state poisoned");
        Ok(state.diagnostics())
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
        let state = self.state.lock().expect("rolling state poisoned");
        let mut items: Vec<_> = Vec::new();
        if let Some(summary) = &state.summary {
            items.push(summary.summary());
        }
        items.extend(state.records.iter().map(Record::summary));
        if items.len() > limit {
            items.truncate(limit);
        }
        Ok(items)
    }

    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        let state = self.state.lock().expect("rolling state poisoned");
        serde_json::to_value(&*state)
            .map_err(|e| AgentError::Internal(format!("rolling checkpoint: {e}")))
    }

    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        let restored: RollingState = serde_json::from_value(data)
            .map_err(|e| AgentError::Internal(format!("rolling restore: {e}")))?;
        *self.state.lock().expect("rolling state poisoned") = restored;
        Ok(())
    }
}

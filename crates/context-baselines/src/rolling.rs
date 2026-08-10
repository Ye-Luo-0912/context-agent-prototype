//! Baseline B: append + periodic summary at a threshold.
//!
//! Everything is appended like baseline A, but when the retained history
//! crosses `summary_threshold_tokens` the oldest records — those strictly
//! older than the newest `keep_most_recent_tokens` verbatim window — are
//! folded into a single placeholder summary marker. This models the classic
//! "summarize when the window fills" baseline; the marker stands in for an
//! LLM summary because the metric of interest here is token growth, not
//! summary quality.

use std::sync::Mutex as StdMutex;

use agent_contracts::{
    AgentError, AgentResult, AttentionState, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextScope,
    ContextSelection, ContextStateTransition, MaterializedContext, ScopeId, ScopeKind,
    ScoreBreakdown,
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
        active_diagnostics(&self.records, self.summary.as_ref(), self.collapsed)
    }
}

/// Baseline B context engine: append, then collapse the oldest history into a
/// rolling summary once a token threshold is crossed.
pub struct RollingSummaryEngine {
    state: StdMutex<RollingState>,
    config: RollingConfig,
}

impl RollingSummaryEngine {
    pub fn new() -> Self {
        Self::with_config(RollingConfig::default())
    }

    pub fn with_config(config: RollingConfig) -> Self {
        Self {
            state: StdMutex::new(RollingState::default()),
            config,
        }
    }
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
        let mut state = self.state.lock().expect("rolling state poisoned");
        let mut transitions: Vec<ContextStateTransition> = Vec::new();
        while state.total_tokens() > self.config.summary_threshold_tokens {
            // How many oldest records fall outside the newest verbatim
            // window (measured in tokens, newest first)?
            let mut kept_tokens = 0usize;
            let mut fold_candidates = 0usize;
            for record in state.records.iter().rev() {
                if kept_tokens >= self.config.keep_most_recent_tokens {
                    fold_candidates += 1;
                }
                kept_tokens += approx_tokens(&record.content);
            }
            if fold_candidates == 0 {
                // Even the verbatim window alone is over the threshold; the
                // summary approach cannot help further. Stop folding.
                break;
            }

            for _ in 0..fold_candidates {
                let record = state.records.remove(0);
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
            state.summary = Some(Record {
                id: summary_id,
                kind: ContextKind::Summary,
                scope: ContextScope::Task,
                content: format!(
                    "Earlier context: {} prior messages collapsed (goals, decisions, tool results). The work below is current.",
                    state.collapsed
                ),
                created_turn: 0,
                source: Some("rolling summary".into()),
            });
        }

        Ok(ContextMaintenanceReport {
            archived: transitions.len(),
            turn: state.turn,
            transitions,
            diagnostics: state.diagnostics(),
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

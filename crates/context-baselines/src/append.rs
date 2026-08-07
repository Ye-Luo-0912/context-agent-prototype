//! Baseline A: append-only transcript.
//!
//! Every message and tool observation is retained forever; `build_snapshot`
//! returns the full history. There is no lifecycle maintenance — nothing
//! leaves the working set, so input tokens grow linearly with the task. The
//! experiment harness measures how quickly this crosses the budget.

use std::sync::Mutex as StdMutex;

use agent_contracts::{
    AgentError, AgentResult, ContextBuildRequest, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextSelection,
    ContextSnapshot, ScoreBreakdown,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::shared::{
    Record, active_diagnostics, approx_tokens, build_messages, records_for_ingress,
};

#[derive(Debug, Default, Serialize, Deserialize)]
struct AppendState {
    records: Vec<Record>,
    turn: u64,
}

/// Baseline A context engine: append-only, no maintenance.
pub struct AppendOnlyEngine {
    state: StdMutex<AppendState>,
}

impl AppendOnlyEngine {
    pub fn new() -> Self {
        Self {
            state: StdMutex::new(AppendState::default()),
        }
    }
}

impl Default for AppendOnlyEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContextEngine for AppendOnlyEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let mut state = self.state.lock().expect("append-only state poisoned");
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
        let state = self.state.lock().expect("append-only state poisoned");
        Ok(ContextMaintenanceReport {
            diagnostics: active_diagnostics(&state.records, None, 0),
            turn: state.turn,
            ..ContextMaintenanceReport::default()
        })
    }

    async fn build_snapshot(&self, request: ContextBuildRequest) -> AgentResult<ContextSnapshot> {
        let state = self.state.lock().expect("append-only state poisoned");
        let messages = build_messages(
            &request.system_prompt,
            None,
            &state.records,
            &request.current_input,
        );
        let snapshot_tokens: usize = messages.iter().map(|m| approx_tokens(&m.content)).sum();
        let selected = state
            .records
            .iter()
            .map(|record| ContextSelection {
                item_id: record.id,
                score: 1.0,
                approx_tokens: approx_tokens(&record.content),
                reason: "append-only baseline: all history retained".into(),
                breakdown: ScoreBreakdown::default(),
            })
            .collect();
        Ok(ContextSnapshot {
            messages,
            selected,
            approx_tokens: snapshot_tokens,
            diagnostics: active_diagnostics(&state.records, None, 0),
        })
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().expect("append-only state poisoned");
        Ok(active_diagnostics(&state.records, None, 0))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
        let state = self.state.lock().expect("append-only state poisoned");
        let mut items: Vec<_> = state.records.iter().map(Record::summary).collect();
        if items.len() > limit {
            items.truncate(limit);
        }
        Ok(items)
    }

    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        let state = self.state.lock().expect("append-only state poisoned");
        serde_json::to_value(&*state)
            .map_err(|e| AgentError::Internal(format!("append-only checkpoint: {e}")))
    }

    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        let restored: AppendState = serde_json::from_value(data)
            .map_err(|e| AgentError::Internal(format!("append-only restore: {e}")))?;
        *self.state.lock().expect("append-only state poisoned") = restored;
        Ok(())
    }
}

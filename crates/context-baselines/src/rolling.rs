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
    ContextStateTransition, FocusState, MaterializedContext, ScopeId, ScopeKind, ScoreBreakdown,
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
    /// ROLLING-FOCUS：runtime 经 FocusChanged 安装的聚焦任务。恢复侧的
    /// focus 权威校验读 `diagnostics.focus_task_id`；不跟踪时默认 profile
    /// （Rolling）的活动任务检查点会被不可恢复地拒绝。
    #[serde(default)]
    focus: Option<FocusState>,
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
        diagnostics.focus_generation = self.focus.as_ref().map_or(0, |focus| focus.generation);
        diagnostics.focus_task_id = self.focus.as_ref().map(|focus| focus.task_id);
        diagnostics
    }
}

/// Baseline B context engine: append, then collapse the oldest history into a
/// rolling summary once a token threshold is crossed.
pub struct RollingSummaryEngine {
    /// `Arc` 共享给折叠取消守卫：压缩期间 maintain future 被丢弃时，守卫
    /// 需要把移出的记录还回这里，而不是让它们随 future 一起消失。
    state: Arc<StdMutex<RollingState>>,
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
            state: Arc::new(StdMutex::new(RollingState::default())),
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
        // ROLLING-PRIOR：下次折叠输入 = 旧摘要（先入，保证在字符上限内
        // 保留）＋ 本次移出的最旧记录。旧摘要是更早折叠的唯一残余，截掉
        // 它等于无声丢弃已折叠历史。
        //
        // R08：压缩器的输入容量（SUMMARIZER_PRIOR_CAP）只够装下旧摘要
        // 加最前面的若干条旧记录。诚实消费 = 只折叠能**完整**进入输入
        // 条目的记录——它们全部被压缩器读到，覆盖声明与输入一致；装不
        // 下的旧记录留在 working set（仍是可见/可恢复残余），绝不带着
        // "未读也退出工作集"的覆盖声明移出。全部装不下（例如单条超限）
        // 则本次不折，避免"宣称覆盖却没消费"。
        let mut prior = String::new();
        if let Some(summary) = &state.summary {
            prior.push_str(&summary.content);
            prior.push('\n');
        }
        let merged_prior_summary = state.summary.is_some();
        let prior_chars = prior.chars().count();
        let mut consumed = 0usize;
        let mut input_chars = prior_chars;
        for record in state.records.iter().take(fold_candidates) {
            let chars = record.content.chars().count() + 1;
            if input_chars.saturating_add(chars) > SUMMARIZER_PRIOR_CAP {
                break; // 装不下的记录留在工作集，下轮或按引用恢复
            }
            input_chars = input_chars.saturating_add(chars);
            consumed += 1;
        }
        if consumed == 0 {
            // 最旧记录本身超过输入容量：无法诚实折叠，保留工作集。
            return None;
        }
        let mut folded_records = Vec::with_capacity(consumed);
        let mut transitions = Vec::new();
        for _ in 0..consumed {
            let record = state.records.remove(0);
            prior.push_str(&record.content);
            prior.push('\n');
            state.collapsed += 1;
            folded_records.push(record.clone());
            transitions.push(ContextStateTransition {
                item_id: record.id,
                kind: record.kind,
                scope: record.scope,
                from: AttentionState::Active,
                to: AttentionState::Archived,
                turn: state.turn,
                reason:
                    "collapsed into rolling summary (baseline B, fully consumed by the compactor)"
                        .into(),
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
            folded_now: consumed,
            merged_prior_summary,
            summary_id,
            transitions,
            restore: FoldRestore {
                state: Arc::clone(&self.state),
                records: folded_records,
                collapsed_delta: consumed,
                armed: true,
            },
        })
    }

    async fn compact_fold(&self, job: &FoldJob) -> Option<CompactionOutput> {
        let fallback = fallback_marker(job.collapsed, &job.prior);
        let Some(compactor) = &self.compactor else {
            return Some(CompactionOutput {
                text: fallback,
                ..CompactionOutput::default()
            });
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
                Some(output)
            }
            // 压缩失败：不写占位覆盖旧摘要。返回 None 让调用方丢弃 job，
            // 守卫把移出的记录还回 working set，折叠前状态完整保留，
            // 下一个维护触发再试。
            Err(_) => None,
        }
    }
}

/// 一次折叠的取消守卫：job 未提交（压缩失败或 maintain future 在压缩
/// await 点被丢弃）时，把移出的记录按原顺序还回 working set 并回退
/// `collapsed` 计数；旧摘要只在提交时才被覆盖，因此折叠前状态不丢。
struct FoldRestore {
    state: Arc<StdMutex<RollingState>>,
    /// 本次移出的记录（最旧优先），归还时插回队首。
    records: Vec<Record>,
    collapsed_delta: usize,
    armed: bool,
}

impl FoldRestore {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FoldRestore {
    fn drop(&mut self) {
        if !self.armed || (self.records.is_empty() && self.collapsed_delta == 0) {
            return;
        }
        // 锁中毒说明别的线程已在 panic 路径上；守卫不在 Drop 里二次 panic。
        if let Ok(mut state) = self.state.lock() {
            state.records.splice(0..0, self.records.drain(..));
            state.collapsed = state.collapsed.saturating_sub(self.collapsed_delta);
        }
    }
}

struct FoldJob {
    prior: String,
    collapsed: usize,
    /// 本次新移出的记录数（`collapsed` 是累计值）。
    folded_now: usize,
    merged_prior_summary: bool,
    summary_id: agent_contracts::ContextItemId,
    transitions: Vec<ContextStateTransition>,
    restore: FoldRestore,
}

/// 摘要的来源覆盖记录：它合并了哪些输入（本次折叠记录＋是否有旧摘要）。
/// R08：这里的 `folded_now` 是**实际完整进入压缩输入并退役**的记录数
/// （`F ⊆ I`）；未进入输入容量、仍留在 working set 的记录不算覆盖——
/// 覆盖声明绝不超出压缩器真正消费的范围。
fn summary_source(folded_now: usize, merged_prior_summary: bool) -> String {
    if merged_prior_summary {
        format!(
            "rolling summary (covers {folded_now} consumed records + prior summary; records beyond the compactor input capacity stay in the working set)"
        )
    } else {
        format!(
            "rolling summary (covers {folded_now} consumed records; records beyond the compactor input capacity stay in the working set)"
        )
    }
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
        // ROLLING-FOCUS：只跟踪聚焦身份，不铸造 Goal 历史记录（当前轮
        // 表示仍归 TurnFrame，见 shared::records_for_ingress 的注释）。
        match &ingress {
            ContextIngress::FocusChanged { focus } => {
                let mut focus = focus.clone();
                focus.generation += 1;
                state.focus = Some(focus);
            }
            ContextIngress::FocusCleared => {
                state.focus = None;
            }
            ContextIngress::TaskCompleted { task_id, .. } => {
                let completed = task_id.or_else(|| state.focus.as_ref().map(|focus| focus.task_id));
                if let Some(completed) = completed
                    && state.focus.as_ref().map(|focus| focus.task_id) == Some(completed)
                {
                    state.focus = None;
                }
            }
            _ => {}
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
        // StdMutex。记录随 job 移出 working set，但由 FoldRestore 守卫持有：
        // 压缩失败或 future 被丢弃时按原样归还；只有压缩结果写回后才撤防。
        let mut transitions: Vec<ContextStateTransition> = Vec::new();
        let mut pass_in = 0u64;
        let mut pass_out = 0u64;
        let mut compactions = Vec::new();
        while let Some(mut job) = self.take_fold_job() {
            let Some(compacted) = self.compact_fold(&job).await else {
                // 压缩失败：job 在此丢弃，守卫归还记录、旧摘要未被触碰，
                // 折叠前状态保留。本回合不再重试同一折叠，避免对失败
                // 压缩器空转。
                break;
            };
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
                    source: Some(summary_source(job.folded_now, job.merged_prior_summary)),
                });
            }
            // 摘要已写回：撤防守卫，随后 job 丢弃不再归还记录。
            job.restore.disarm();
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

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        let mut state = self.state.lock().expect("rolling state poisoned");
        state.materialization_revision =
            state
                .materialization_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::Internal("context materialization id is exhausted".into())
                })?;
        let prior = crate::shared::bounded_prior(
            &state.records,
            state.turn,
            query.hints.max_selected_items,
        );
        let items = materialized_items(&prior, state.summary.as_ref(), state.turn);
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
                ..Default::default()
            });
        }
        selected.extend(prior.iter().map(|record| ContextSelection {
            item_id: record.id,
            score: 1.0,
            approx_tokens: approx_tokens(&record.content),
            reason: "append + rolling summary baseline: prior history".into(),
            breakdown: ScoreBreakdown::default(),
            ..Default::default()
        }));
        Ok(MaterializedContext {
            materialization_id: state.materialization_revision,
            focus: state.focus.clone(),
            task: None,
            items,
            external: agent_contracts::ContextMapView::default(),
            selected,
            approx_tokens: approx_tokens_total,
            foreground: Vec::new(),
            required_item_ids: Vec::new(),
            required_misses: Default::default(),
            optional_misses: Default::default(),
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

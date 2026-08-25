//! Convergence Bench（评审第 29 条）：四个确定性 scripted-model 场景，
//! 在真实 runtime + 真实工具表面上验证 Execution Convergence 语义。
//! 不依赖 live provider，可在 CI 里反复运行：
//!
//! - `retry_domain`：process.run program-not-found 后的同参数重试被
//!   无派发拒绝（CONV-02 可证等价重试域）；
//! - `operational_evidence`：同版本重复观察入前沿并计 RedundantEvidence
//!   （CONV-01）；
//! - `protocol_body`：正文被 turn checkpoint 截掉后由当轮缓存回注
//!   （PROTO-EVID-01）；
//! - `verification_reuse`：生产 `verify.run` 的同一 exact PASS 第二次
//!   请求不再启动进程，但仍结算完整工具结果。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, ContextEngine, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEventEnvelope, ToolCall, ToolSpec,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use tempfile::TempDir;

use crate::metrics::aggregate_metrics;
use crate::mock_model::ScriptedModel;

/// 审批策略：全放行，被测对象是收敛语义而非审批门。
struct AllowAllGate;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &agent_contracts::CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvergenceBenchReport {
    pub scenario: &'static str,
    pub passed: bool,
    pub detail: String,
}

impl ConvergenceBenchReport {
    fn new(scenario: &'static str, passed: bool, detail: String) -> Self {
        Self {
            scenario,
            passed,
            detail,
        }
    }
}

/// 包装 ScriptedModel：记录是否出现过回注标记（PROTO-EVID-01 断言面）。
struct BodyProbeModel {
    inner: ScriptedModel,
    restored_seen: AtomicBool,
}

impl BodyProbeModel {
    fn new(inner: ScriptedModel) -> Self {
        Self {
            inner,
            restored_seen: AtomicBool::new(false),
        }
    }

    fn restored_seen(&self) -> bool {
        self.restored_seen.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelTransport for BodyProbeModel {
    fn capabilities(&self) -> agent_contracts::ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        if request
            .messages
            .iter()
            .any(|message| message.content.contains("RESTORED TURN BODIES"))
        {
            self.restored_seen.store(true, Ordering::SeqCst);
        }
        self.inner.complete(request).await
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments,
    }
}

async fn run_session(
    workspace_root: &Path,
    model: Arc<dyn ModelTransport>,
    turns: &[&str],
) -> anyhow::Result<Vec<RuntimeEventEnvelope>> {
    let approval: Arc<dyn ApprovalGate> = Arc::new(AllowAllGate);
    let workspace = agent_workspace::Workspace::open(workspace_root).await?;
    let verification_recipes = tool_runtime::VerificationRecipes::discover(&workspace);
    // 收敛基准需要显式运行 process.run；默认表面不含它。
    let lifecycle = tool_runtime::ToolLifecycleConfig {
        always_loaded: vec![
            "fs.list".to_string(),
            "fs.read".to_string(),
            "process.run".to_string(),
        ],
        ..Default::default()
    };
    let tools: Arc<dyn agent_contracts::ToolDispatcher> = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            lifecycle,
            verification_recipes.clone(),
        ),
    );
    let context_engine: Arc<dyn ContextEngine> =
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
    let composed = compose(ComposeConfig {
        workspace,
        context_engine,
        model,
        approval,
        base_tools: tools,
        capability_aware: false,
        journal: None,
        artifact_store: None,
        output_broker: None,
        max_tool_rounds: Some(32),
        project_task_progress: true,
        project_completion_opportunity: false,
        host_policies: Some(Arc::new(
            agent_compose::HostToolPolicyRegistry::with_builtins_and_verification(
                &verification_recipes,
            )
            .map_err(anyhow::Error::msg)?,
        )),
    })
    .await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;

    let mut capture = Vec::new();
    for text in turns {
        composed.handle().user_message((*text).to_string()).await?;
        // 等待本轮结束：TurnCompleted 或错误出现即认为该轮已收尾。
        loop {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(30), events.recv())
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for events"))??;
            let done = matches!(
                envelope.event,
                agent_contracts::RuntimeEvent::TurnCompleted
                    | agent_contracts::RuntimeEvent::Error { .. }
                    | agent_contracts::RuntimeEvent::TurnCommitFailed { .. }
            );
            capture.push(envelope);
            if done {
                break;
            }
        }
    }
    composed.shutdown().await?;
    while let Ok(envelope) = events.try_recv() {
        capture.push(envelope);
    }
    Ok(capture)
}

async fn seed_workspace(files: &[(&str, &str)]) -> anyhow::Result<TempDir> {
    let dir = TempDir::new()?;
    for (path, content) in files {
        let full = dir.path().join(path);
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(full, content).await?;
    }
    Ok(dir)
}

fn count_started(events: &[RuntimeEventEnvelope], tool: &str) -> usize {
    events
        .iter()
        .filter(|envelope| {
            matches!(
                &envelope.event,
                agent_contracts::RuntimeEvent::ToolStarted { call }
                    if call.name == tool
            )
        })
        .count()
}

fn failure_classes(events: &[RuntimeEventEnvelope], tool: &str) -> HashMap<String, usize> {
    let mut classes = HashMap::new();
    for envelope in events {
        if let agent_contracts::RuntimeEvent::ToolFinished { output } = &envelope.event
            && output.tool_name == tool
            && let Some(class) = output.failure_class()
        {
            *classes.entry(class.as_str().to_string()).or_default() += 1;
        }
    }
    classes
}

/// 场景一（CONV-02）：program-not-found 后同参数重试必须被无派发拒绝；
/// 换参数的调用正常派发。
pub async fn scenario_retry_domain() -> anyhow::Result<ConvergenceBenchReport> {
    let dir = seed_workspace(&[("src/main.rs", "fn main() {}\n")]).await?;
    let script = vec![
        tool_call(
            "c1",
            "process.run",
            serde_json::json!({ "argv": ["definitely_missing_prog_xyz"] }),
        ),
        // 同参数重试：应命中可证等价重试域，无派发拒绝。
        tool_call(
            "c2",
            "process.run",
            serde_json::json!({ "argv": ["definitely_missing_prog_xyz"] }),
        ),
    ];
    let model = Arc::new(ScriptedModel::new(script, "retry domain done"));
    let events = run_session(dir.path(), model, &["reproduce the retry loop"]).await?;
    let started = count_started(&events, "process.run");
    let classes = failure_classes(&events, "process.run");
    let duplicate_refused_without_dispatch = started == 1
        && classes.get("duplicate_no_progress").copied() == Some(1)
        && classes.get("path_not_found").copied() == Some(1);
    Ok(ConvergenceBenchReport::new(
        "retry_domain",
        duplicate_refused_without_dispatch,
        format!("process.run started={started}, failure_classes={classes:?}"),
    ))
}

/// 场景二（CONV-01）：同版本重复观察记 RedundantEvidence；首次观察是
/// 可证明推进。
pub async fn scenario_operational_evidence() -> anyhow::Result<ConvergenceBenchReport> {
    let dir = seed_workspace(&[("src/main.rs", "fn main() {}\n")]).await?;
    let list_args = || serde_json::json!({ "path": ".", "depth": 1 });
    let read_args = || serde_json::json!({ "path": "src/main.rs" });
    let script = vec![
        tool_call("c1", "fs.list", list_args()),
        tool_call("c2", "fs.read", read_args()),
        tool_call("c3", "fs.list", list_args()),
        tool_call("c4", "fs.read", read_args()),
    ];
    let model = Arc::new(ScriptedModel::new(script, "evidence done"));
    let events = run_session(dir.path(), model, &["survey the workspace"]).await?;
    let metrics = aggregate_metrics(&events);
    let passed = metrics.frontier_advances >= 2 && metrics.redundant_evidence_calls == 2;
    let frontier_log: Vec<String> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            agent_contracts::RuntimeEvent::ExecutionFrontier {
                delta,
                actions_since_frontier_advance,
                invalidated,
                ..
            } => {
                let delta_name = format!("{delta:?}");
                Some(format!(
                    "{delta_name}(debt={invalidated},no_advance={actions_since_frontier_advance})"
                ))
            }
            _ => None,
        })
        .collect();
    let mut list_meta = String::new();
    for envelope in &events {
        if let agent_contracts::RuntimeEvent::ToolFinished { output } = &envelope.event
            && output.tool_name == "fs.list"
        {
            list_meta.push_str(&format!(
                "[path={:?} rev={:?}] ",
                output.metadata.get("path"),
                output.metadata.get("revision"),
            ));
        }
    }
    Ok(ConvergenceBenchReport::new(
        "operational_evidence",
        passed,
        format!(
            "frontier_advances={}, redundant_evidence_calls={}, no_advance_peak={}, \
             deltas=[{}], list_meta={list_meta}",
            metrics.frontier_advances,
            metrics.redundant_evidence_calls,
            metrics.frontier_no_advance_peak,
            frontier_log.join(", "),
        ),
    ))
}

/// 场景三（PROTO-EVID-01）：首个 fs.read 正文被 checkpoint 截掉后，
/// 同一身份由当轮缓存回注到下一轮请求。
pub async fn scenario_protocol_body() -> anyhow::Result<ConvergenceBenchReport> {
    let body = "fn secret_body() {}\n";
    let mut script = vec![tool_call(
        "c0",
        "fs.read",
        serde_json::json!({ "path": "src/main.rs" }),
    )];
    // 填充交换把第一次读取挤出保留尾（TURN_FRAME_KEEP_EXCHANGES=6）。
    for index in 1..8 {
        script.push(tool_call(
            &format!("c{index}"),
            "fs.read",
            serde_json::json!({ "path": "src/filler.md" }),
        ));
    }
    let probe = Arc::new(BodyProbeModel::new(ScriptedModel::new(
        script,
        "protocol done",
    )));
    let dir = seed_workspace(&[("src/main.rs", body), ("src/filler.md", "filler\n")]).await?;
    let events = run_session(dir.path(), probe.clone(), &["inspect main"]).await?;
    let rereads = count_started(&events, "fs.read");
    // 脚本共 8 次读取：首次 + 7 个填充交换把首次挤出保留尾。
    let passed = probe.restored_seen() && rereads == 8;
    Ok(ConvergenceBenchReport::new(
        "protocol_body",
        passed,
        format!(
            "restored_turn_bodies_seen={}, fs_read_starts={rereads}",
            probe.restored_seen()
        ),
    ))
}

/// 场景四：真实 builtin dispatcher + Core authority + `rustc` recipe。
/// 第一次成功记录 PASS；同 directive/current world 的第二次调用复用
/// 结果，不能再产生 ToolStarted/子进程。
pub async fn scenario_verification_reuse() -> anyhow::Result<ConvergenceBenchReport> {
    let dir = seed_workspace(&[("src/main.rs", "fn main() {}\n")]).await?;
    let arguments = || {
        serde_json::json!({
            "recipe_id": "rust.compile-tests:src/main.rs"
        })
    };
    let model = Arc::new(ScriptedModel::new(
        vec![
            tool_call("v1", "verify.run", arguments()),
            tool_call("v2", "verify.run", arguments()),
        ],
        "verification reuse done",
    ));
    let events = run_session(dir.path(), model, &["compile the exact test target twice"]).await?;
    let started = count_started(&events, "verify.run");
    let finished = events
        .iter()
        .filter(|envelope| {
            matches!(
                &envelope.event,
                agent_contracts::RuntimeEvent::ToolFinished { output }
                    if output.tool_name == "verify.run" && output.ok
            )
        })
        .count();
    let metrics = aggregate_metrics(&events);
    let trace = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            agent_contracts::RuntimeEvent::ToolStarted { call } if call.name == "verify.run" => {
                Some(format!("started:{}", call.id))
            }
            agent_contracts::RuntimeEvent::ToolFinished { output }
                if output.tool_name == "verify.run" =>
            {
                Some(format!(
                    "finished:{}:{}:{}",
                    output.call_id, output.ok, output.summary
                ))
            }
            agent_contracts::RuntimeEvent::ExecutionVerificationPass { kind, .. } => {
                Some(format!("pass:{kind:?}"))
            }
            agent_contracts::RuntimeEvent::Error { message } => Some(format!("error:{message}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let passed = started == 1
        && finished == 2
        && metrics.verification_pass_recorded == 1
        && metrics.verification_pass_reused == 1;
    Ok(ConvergenceBenchReport::new(
        "verification_reuse",
        passed,
        format!(
            "verify.run started={started}, finished={finished}, pass_recorded={}, pass_reused={}, trace=[{trace}]",
            metrics.verification_pass_recorded, metrics.verification_pass_reused,
        ),
    ))
}

/// 运行全部四个确定性场景；任一失败即整体失败。
pub async fn run_convergence_bench() -> anyhow::Result<Vec<ConvergenceBenchReport>> {
    let reports = vec![
        scenario_retry_domain().await?,
        scenario_operational_evidence().await?,
        scenario_protocol_body().await?,
        scenario_verification_reuse().await?,
    ];
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convergence Bench 自身作为测试运行（评审第 29 条的验收门）。
    #[tokio::test]
    async fn all_four_scenarios_pass() {
        let reports = run_convergence_bench().await.expect("bench runs");
        for report in &reports {
            assert!(
                report.passed,
                "scenario {} failed: {}",
                report.scenario, report.detail
            );
        }
    }
}

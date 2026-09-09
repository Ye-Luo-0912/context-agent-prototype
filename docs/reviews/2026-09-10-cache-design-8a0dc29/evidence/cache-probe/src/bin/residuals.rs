//! Deterministic counterexamples on the reviewed code. No provider/network calls.
use agent_contracts::*;
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use context_baselines::{RollingConfig, RollingSummaryEngine};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ContextEngine for Gate {
    async fn ingest(&self, _: ContextIngress) -> AgentResult<()> { Ok(()) }
    async fn maintain(&self, trigger: ContextMaintenanceTrigger) -> AgentResult<ContextMaintenanceReport> {
        if trigger == ContextMaintenanceTrigger::AfterModel {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _: ContextQuery) -> AgentResult<MaterializedContext> { Ok(MaterializedContext::default()) }
    async fn open_scope(&self, _: ScopeKind, _: Option<ScopeId>) -> AgentResult<ScopeId> { Ok(ScopeId::new()) }
    async fn close_scope(&self, _: ScopeId) -> AgentResult<Vec<ContextStateTransition>> { Ok(vec![]) }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> { Ok(ContextDiagnostics::default()) }
    async fn inspect(&self, _: usize) -> AgentResult<Vec<ContextItemSummary>> { Ok(vec![]) }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> { Ok(serde_json::Value::Null) }
    async fn restore(&self, _: serde_json::Value) -> AgentResult<()> { Ok(()) }
}

struct FinalModel;
#[async_trait::async_trait]
impl ModelTransport for FinalModel {
    fn capabilities(&self) -> ModelCapabilities { ModelCapabilities::default() }
    async fn complete(&self, _: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput { content:"bounded final".into(), tool_calls:vec![], usage:Default::default() })
    }
}

struct NoTools;
#[async_trait::async_trait]
impl ToolDispatcher for NoTools {
    fn specs(&self) -> Vec<ToolSpec> { vec![] }
    async fn execute(&self, _: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(AgentError::Tool("no tools in this fixture".into()))
    }
}

struct NeverCalledCompactor;
#[async_trait::async_trait]
impl BoundedCompactor for NeverCalledCompactor {
    async fn compact(&self, _: CompactionRequest) -> AgentResult<CompactionOutput> {
        panic!("zero compactor budget must not execute the compactor")
    }
}

#[tokio::main]
async fn main() {
    let gate = Arc::new(Gate { entered:tokio::sync::Notify::new(), release:tokio::sync::Notify::new() });
    let services = Arc::new(RuntimeServices::new(CoreAuthorityConfig::default(), gate.clone(),
        Arc::new(FinalModel), Arc::new(NoTools), Arc::new(PolicyApprovalGate::read_only()), None));
    let (handle, actor) = spawn_runtime(services);
    handle.start().await.unwrap();
    handle.set_focus("review maintenance cancellation".into()).await.unwrap();
    handle.user_message("finish".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await.unwrap();
    let cancel_answered = tokio::time::timeout(Duration::from_millis(300), handle.cancel_turn()).await.is_ok();
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), handle.stop()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(3), actor).await.unwrap().unwrap();
    assert!(!cancel_answered, "counterexample changed: review W04 residual again");

    let dir = tempfile::tempdir().unwrap();
    let workspace = agent_workspace::Workspace::open(dir.path()).await.unwrap();
    let run_id = RunId::new();
    let data = format!("{}\n{}", "x".repeat(3 * 1024 * 1024), "tail\n".repeat(100));
    let reference = workspace.write_artifact(run_id, "probe", "log", data.as_bytes()).await.unwrap();
    let dispatcher = tool_runtime::BuiltinToolDispatcher::new(workspace).unwrap();
    let ToolOutcome::Value(output) = dispatcher.execute(ToolExecutionRequest {
        run_id, call:ToolCall { id:"read".into(), name:"artifact.read".into(),
            arguments:json!({"reference":reference,"start_line":1,"end_line":101}) },
        effect_context:None, cancel:CancellationToken::new(),
    }).await.unwrap() else { panic!("read must return a value") };
    // Ignore the small line-number framing: the capture already exceeds its cap.
    assert!(output.model_content.len() > 2 * 1024 * 1024 + 100);

    let rolling = RollingSummaryEngine::with_config(RollingConfig {
        summary_threshold_tokens:10, keep_most_recent_tokens:2, max_compactor_calls_per_maintain:0,
    }).with_compactor(Arc::new(NeverCalledCompactor));
    rolling.ingest(ContextIngress::AssistantMessage { content:"old constraint ".repeat(10) }).await.unwrap();
    rolling.ingest(ContextIngress::AssistantMessage { content:"recent text ".repeat(10) }).await.unwrap();
    let report = rolling.maintain(ContextMaintenanceTrigger::BeforeModel).await.unwrap();
    let checkpoint = rolling.checkpoint().await.unwrap();
    let remaining = checkpoint["records"].as_array().unwrap().len();
    assert_eq!(remaining, 2);
    assert_eq!(report.deferred_folds, 0, "counterexample changed: review deferred reporting again");
    println!("{}", serde_json::to_string_pretty(&json!({
        "after_model_maintenance":{"gate_entered":true,"cancel_answered_within_300ms":cancel_answered,"actor_cleanly_joined":true},
        "artifact_capture":{"source_bytes":data.len(),"capture_cap_bytes":2 * 1024 * 1024,
            "model_content_bytes":output.model_content.len(),"metadata":output.metadata},
        "maintenance_deferral":{"compactor_budget":0,"records_remaining":remaining,"deferred_folds":report.deferred_folds},
        "real_provider_calls":0
    })).unwrap());
}

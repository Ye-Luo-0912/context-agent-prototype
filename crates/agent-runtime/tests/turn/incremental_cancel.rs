use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, ScopeId,
    ScopeKind, ToolCall, TurnCancelAck,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;
use tokio::sync::Notify;

use super::adaptive_history::ReadFixtures;

struct GatedRealContext {
    engine: context_simple::SimpleContextEngine,
    entered: Notify,
    release: Notify,
    attempted_observations: AtomicUsize,
    fail_restore: bool,
}

#[async_trait::async_trait]
impl ContextEngine for GatedRealContext {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::ToolObservation { .. })
            && self.attempted_observations.fetch_add(1, Ordering::SeqCst) == 1
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.engine.ingest(ingress).await
    }

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.engine.maintain(trigger).await
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.engine.materialize(query).await
    }

    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        self.engine.open_scope(kind, parent).await
    }

    async fn close_scope(&self, scope: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.engine.close_scope(scope).await
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        self.engine.diagnostics().await
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.engine.inspect(limit).await
    }

    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        self.engine.checkpoint().await
    }

    async fn restore(&self, checkpoint: serde_json::Value) -> AgentResult<()> {
        if self.fail_restore {
            return Err(agent_contracts::AgentError::Context(
                "injected restore failure".into(),
            ));
        }
        self.engine.restore(checkpoint).await
    }
}

#[derive(Default)]
struct TwoReadsModel;

#[async_trait::async_trait]
impl ModelTransport for TwoReadsModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            ..Default::default()
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: (0..2)
                .map(|index| ToolCall {
                    id: format!("read-{index}"),
                    name: "fs.read".into(),
                    arguments: json!({"path": format!("api/module-{index}.py")}),
                })
                .collect(),
            usage: Default::default(),
        })
    }
}

async fn cancelled_incremental_ingest(fail_restore: bool) {
    let directory = tempfile::tempdir().unwrap();
    let context = Arc::new(GatedRealContext {
        engine: context_simple::SimpleContextEngine::new(context_simple::SimpleContextConfig {
            context_store_dir: Some(directory.path().join("context-store")),
            ..Default::default()
        }),
        entered: Notify::new(),
        release: Notify::new(),
        attempted_observations: AtomicUsize::new(0),
        fail_restore,
    });
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(TwoReadsModel),
        Arc::new(ReadFixtures {
            expose_resource: false,
            body_bytes: 0,
            first_observation: None,
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let runtime = RuntimeInstance::spawn(host, services);
    let handle = runtime.handle();
    handle.start().await.unwrap();
    handle
        .set_focus("inspect two modules".into())
        .await
        .unwrap();
    handle
        .user_message("read both modules".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), context.entered.notified())
        .await
        .unwrap();
    let cancellation = tokio::time::timeout(Duration::from_secs(20), handle.cancel_turn())
        .await
        .unwrap();
    if fail_restore {
        assert!(
            cancellation.is_err(),
            "failed restore must not acknowledge cancellation"
        );
        assert_eq!(
            handle
                .status_snapshot()
                .await
                .unwrap()
                .continue_readiness
                .reason,
            agent_runtime::ContinueReason::RecoveryRequired,
        );
    } else {
        assert!(matches!(
            cancellation.unwrap(),
            TurnCancelAck::Cancelled { .. }
        ));
        let diagnostics = context.diagnostics().await.unwrap();
        assert_eq!(
            diagnostics.tool_round, 2,
            "each accepted result survives exactly once"
        );
        assert_eq!(
            context.attempted_observations.load(Ordering::SeqCst),
            4,
            "first two attempts are rolled back; cancellation preserves both once"
        );
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_joins_partial_real_ingest_and_preserves_each_accepted_result_once() {
    cancelled_incremental_ingest(false).await;
}

#[tokio::test]
async fn failed_partial_ingest_restore_fences_later_mutation() {
    cancelled_incremental_ingest(true).await;
}

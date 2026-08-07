//! Module host tests: typed capability registration and lookup, duplicate
//! rejection, and lifecycle ordering.

use std::sync::{Arc, Mutex};

use agent_contracts::{
    AgentResult, ApprovalGate, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport,
    ToolDispatcher, ToolExecutionRequest, ToolOutput, ToolSpec,
};
use agent_runtime::{
    APPROVAL_POLICY, CapabilityId, ContextModule, ModelModule, Module, ModuleHost, ServiceRegistry,
    ToolModule,
};

#[derive(Debug)]
struct StubContextEngine;

#[async_trait::async_trait]
impl ContextEngine for StubContextEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            focus: None,
            items: Vec::new(),
            selected: Vec::new(),
            approx_tokens: 0,
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct StubModel;

#[async_trait::async_trait]
impl ModelTransport for StubModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[derive(Debug)]
struct StubTools;

#[async_trait::async_trait]
impl ToolDispatcher for StubTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        Err(agent_contracts::AgentError::Tool("stub".into()))
    }
}

#[derive(Debug)]
struct StubApproval;

#[async_trait::async_trait]
impl ApprovalGate for StubApproval {
    async fn authorize(
        &self,
        _call: &agent_contracts::ToolCall,
        _spec: &ToolSpec,
    ) -> AgentResult<agent_contracts::ApprovalDecision> {
        Ok(agent_contracts::ApprovalDecision::Allow)
    }
}

#[tokio::test]
async fn host_registers_and_looks_up_typed_capabilities() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))))
        .unwrap();
    host.add_module(Arc::new(ModelModule::new(Arc::new(StubModel))))
        .unwrap();
    host.add_module(Arc::new(ToolModule::new(Arc::new(StubTools))))
        .unwrap();
    host.add_module(Arc::new(agent_runtime::ApprovalModule::new(Arc::new(
        StubApproval,
    ))))
    .unwrap();
    host.start().await.unwrap();

    // Typed lookups return the exact capability and stay usable.
    let engine = host.registry().context_service().unwrap();
    let diagnostics = engine.diagnostics().await.unwrap();
    assert_eq!(diagnostics.total_items, 0);
    assert!(host.registry().model_provider().is_ok());
    assert!(host.registry().tool_provider().is_ok());
    assert!(host.registry().approval_policy().is_ok());
    // Optional capabilities are absent unless a module published them.
    assert!(host.registry().event_store().unwrap().is_none());
    assert!(host.registry().artifact_store().unwrap().is_none());

    host.stop().await.unwrap();
}

#[tokio::test]
async fn host_rejects_duplicate_capability_claims() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))))
        .unwrap();
    let duplicate = host.add_module(Arc::new(ContextModule::new(Arc::new(StubContextEngine))));
    assert!(
        duplicate.is_err(),
        "a second context module must be rejected at composition time"
    );
    assert!(
        duplicate
            .unwrap_err()
            .to_string()
            .contains("already claimed"),
        "the error must name the conflict"
    );
}

#[tokio::test]
async fn host_starts_in_order_and_stops_in_reverse() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();

    struct RecordingModule {
        name: &'static str,
        order: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Module for RecordingModule {
        fn name(&self) -> &'static str {
            self.name
        }
        fn capabilities(&self) -> Vec<CapabilityId> {
            Vec::new()
        }
        fn register(&self, _registry: &mut ServiceRegistry) -> AgentResult<()> {
            Ok(())
        }
        async fn start(&self) -> AgentResult<()> {
            self.order
                .lock()
                .unwrap()
                .push(format!("start:{}", self.name));
            Ok(())
        }
        async fn stop(&self) -> AgentResult<()> {
            self.order
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.name));
            Ok(())
        }
    }

    host.add_module(Arc::new(RecordingModule {
        name: "context",
        order: order.clone(),
    }))
    .unwrap();
    host.add_module(Arc::new(RecordingModule {
        name: "model",
        order: order.clone(),
    }))
    .unwrap();

    host.start().await.unwrap();
    host.stop().await.unwrap();

    let order = order.lock().unwrap().clone();
    assert_eq!(
        order,
        vec![
            "start:context".to_string(),
            "start:model".to_string(),
            "stop:model".to_string(),
            "stop:context".to_string(),
        ]
    );
}

#[tokio::test]
async fn missing_capability_lookup_fails_with_a_clear_error() {
    let host = ModuleHost::new();
    match host.registry().context_service() {
        Ok(_) => panic!("a missing capability must fail"),
        Err(error) => assert!(
            error.to_string().contains("context-service"),
            "the error must name the missing capability, got: {error}"
        ),
    }
    // Module claims are empty for an unregistered id.
    assert!(host.registry().claims(APPROVAL_POLICY).is_none());
}

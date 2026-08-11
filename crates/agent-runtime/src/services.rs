//! The composition seam (MOD-04A): the concrete implementations one run
//! needs, resolved from the module host's typed registry or built directly
//! in tests. A composition root constructs one `RuntimeServices` and hands
//! it to the runtime; the runtime splits it into the kernel's authority
//! inputs. This is the seam the incremental Core migration targets: the
//! kernel should be *given* its services, never construct or schedule
//! them, so a future Core stays a pure authority while all scheduling
//! (context maintenance, model calls, tool lifecycle) lives here in the
//! runtime.

use std::sync::Arc;

use agent_contracts::{
    AgentResult, ApprovalGate, ContextEngine, EventJournal, ModelTransport, ToolDispatcher,
};
use agent_kernel::{AgentKernel, AgentKernelConfig};

use crate::host::ServiceRegistry;

/// The concrete implementations one run needs. The kernel is built from
/// these (authority-only: events, approval, effects, output); scheduling —
/// context maintenance, tool lifecycle, model calls — is the runtime's
/// job, which is why the services live here rather than inside the kernel.
pub struct RuntimeServices {
    pub kernel_config: AgentKernelConfig,
    pub context: Arc<dyn ContextEngine>,
    pub model: Arc<dyn ModelTransport>,
    pub tools: Arc<dyn ToolDispatcher>,
    pub approval: Arc<dyn ApprovalGate>,
    pub journal: Option<Arc<dyn EventJournal>>,
}

impl RuntimeServices {
    /// Build services directly (tests, standalone composition roots). The
    /// argument order matches `AgentKernel::new` so the mechanical move is
    /// a drop-in replacement.
    pub fn new(
        kernel_config: AgentKernelConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn ApprovalGate>,
        journal: Option<Arc<dyn EventJournal>>,
    ) -> Self {
        Self {
            kernel_config,
            context,
            model,
            tools,
            approval,
            journal,
        }
    }

    /// Resolve every service from the module host's typed registry. The
    /// kernel configuration stays with the composition root (the broker,
    /// shadow gate and lease TTL are root decisions, not registry
    /// services).
    pub fn from_registry(
        registry: &ServiceRegistry,
        kernel_config: AgentKernelConfig,
    ) -> AgentResult<Self> {
        Ok(Self {
            kernel_config,
            context: registry.context_service()?,
            model: registry.model_provider()?,
            tools: registry.tool_provider()?,
            approval: registry.approval_policy()?,
            journal: registry.event_store()?,
        })
    }

    /// The kernel this run uses: the four authority seams over the
    /// resolved services. The kernel is rebuilt from the services so a
    /// composition root never constructs the authority facade directly.
    pub fn kernel(&self) -> AgentKernel {
        AgentKernel::new(
            self.kernel_config.clone(),
            self.context.clone(),
            self.model.clone(),
            self.tools.clone(),
            self.approval.clone(),
            self.journal.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AgentError, ContextDiagnostics, ContextIngress, ContextMaintenanceReport,
        ContextMaintenanceTrigger, ContextQuery, ContextStateTransition, MaterializedContext,
        ModelCapabilities, ModelOutput, ModelRequest, ScopeId, ScopeKind, ToolExecutionRequest,
        ToolOutcome, ToolSpec,
    };
    use agent_kernel::PolicyApprovalGate;
    use std::sync::Arc;

    #[derive(Debug)]
    struct StubContext;

    #[async_trait::async_trait]
    impl ContextEngine for StubContext {
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
                materialization_id: 0,
                focus: None,
                items: Vec::new(),
                external: agent_contracts::ContextMapView::default(),
                selected: Vec::new(),
                approx_tokens: 0,
                diagnostics: ContextDiagnostics::default(),
            })
        }
        async fn open_scope(
            &self,
            _kind: ScopeKind,
            _parent: Option<ScopeId>,
        ) -> AgentResult<ScopeId> {
            Ok(ScopeId::new())
        }
        async fn close_scope(
            &self,
            _scope_id: ScopeId,
        ) -> AgentResult<Vec<ContextStateTransition>> {
            Ok(Vec::new())
        }
        async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
            Ok(ContextDiagnostics::default())
        }
        async fn inspect(
            &self,
            _limit: usize,
        ) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
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
        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            Err(AgentError::Tool("stub".into()))
        }
    }

    #[test]
    fn services_build_a_kernel_and_round_trip_the_registry() {
        let services = RuntimeServices::new(
            AgentKernelConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubModel),
            Arc::new(StubTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        );
        let kernel = services.kernel();
        // The kernel facade carries the same identity the run will use.
        assert_eq!(kernel.system_prompt(), services.kernel_config.system_prompt);
        assert_eq!(
            kernel.context_budget_tokens(),
            services.kernel_config.context_budget_tokens
        );

        // A registry that publishes the same services resolves them back.
        let mut registry = ServiceRegistry::new();
        registry
            .register(
                crate::host::CONTEXT_SERVICE,
                "test",
                services.context.clone(),
            )
            .unwrap();
        registry
            .register(crate::host::MODEL_PROVIDER, "test", services.model.clone())
            .unwrap();
        registry
            .register(crate::host::TOOL_PROVIDER, "test", services.tools.clone())
            .unwrap();
        registry
            .register(
                crate::host::APPROVAL_POLICY,
                "test",
                services.approval.clone(),
            )
            .unwrap();
        let resolved = RuntimeServices::from_registry(&registry, AgentKernelConfig::default())
            .expect("every required service is present");
        assert_eq!(resolved.kernel().system_prompt(), kernel.system_prompt());
    }
}

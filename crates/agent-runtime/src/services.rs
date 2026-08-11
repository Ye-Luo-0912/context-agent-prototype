//! The composition seam (MOD-04A): the concrete implementations one run
//! needs, resolved from the module host's typed registry or built directly
//! in tests. A composition root constructs one `RuntimeServices` and hands
//! it to the runtime; the runtime uses it for *all* scheduling — context
//! maintenance and focus transactions, model calls, tool lifecycle and
//! surface scheduling, config access — while the kernel it derives from the
//! services stays authority-only (events, approval, effects, output, and
//! the tool-execution wiring that combines them). This is the seam the
//! incremental Core migration targets: the kernel is *given* its services,
//! never constructs or schedules them, so a future Core stays a pure
//! authority.

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, ContextEngine, ContextGcReport, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, EventJournal, FocusState, MaterializedContext, ModelCapabilities,
    ModelEventSink, ModelOutput, ModelRequest, ModelTransport, ScopeId, ScopeKind, TaskId,
    ToolCatalogEntry, ToolDispatcher, ToolSpec, ToolSurfaceSnapshot,
};
use agent_core::{CoreAuthority, CoreAuthorityConfig};

use crate::host::ServiceRegistry;

/// The concrete implementations one run needs, plus the kernel derived from
/// them. Scheduling — context maintenance, model calls, tool lifecycle —
/// lives here in the runtime; the kernel is the authority facade
/// (`services.kernel()`) the actor consults for events, approval, effects,
/// output and tool-execution wiring.
pub struct RuntimeServices {
    kernel: Arc<CoreAuthority>,
    pub kernel_config: CoreAuthorityConfig,
    pub context: Arc<dyn ContextEngine>,
    pub model: Arc<dyn ModelTransport>,
    pub tools: Arc<dyn ToolDispatcher>,
    pub approval: Arc<dyn ApprovalGate>,
    pub journal: Option<Arc<dyn EventJournal>>,
}

impl RuntimeServices {
    /// Build services directly (tests, standalone composition roots). The
    /// kernel is derived once here, so every caller that later asks for
    /// `kernel()` shares one authority instance (same run id, sequence and
    /// event channel). The model transport is a *scheduling* service: the
    /// kernel (authority facade) does not call the provider, so it is not
    /// part of the kernel's inputs.
    pub fn new(
        kernel_config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn ApprovalGate>,
        journal: Option<Arc<dyn EventJournal>>,
    ) -> Self {
        let kernel = Arc::new(CoreAuthority::new(
            kernel_config.clone(),
            context.clone(),
            tools.clone(),
            approval.clone(),
            journal.clone(),
        ));
        Self {
            kernel,
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
        kernel_config: CoreAuthorityConfig,
    ) -> AgentResult<Self> {
        Ok(Self::new(
            kernel_config,
            registry.context_service()?,
            registry.model_provider()?,
            registry.tool_provider()?,
            registry.approval_policy()?,
            registry.event_store()?,
        ))
    }

    /// The kernel this run uses: the authority facade (events, approval,
    /// effects, output) plus the tool-execution wiring. Shared by the actor
    /// and the spawn seam — one instance per run.
    pub fn kernel(&self) -> Arc<CoreAuthority> {
        self.kernel.clone()
    }

    // --- configuration (moved out of the kernel) ---

    pub fn system_prompt(&self) -> String {
        self.kernel_config.system_prompt.clone()
    }

    pub fn context_budget_tokens(&self) -> usize {
        self.kernel_config.context_budget_tokens
    }

    pub fn max_tool_rounds(&self) -> usize {
        self.kernel_config.max_tool_rounds
    }

    // --- model scheduling (moved out of the kernel) ---

    pub fn model_capabilities(&self) -> ModelCapabilities {
        self.model.capabilities()
    }

    /// One model round: stream the request to the provider. The result is a
    /// value for the actor to validate and commit — nothing is committed
    /// here.
    pub async fn run_model_round(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        self.model.complete_stream(request, sink).await
    }

    // --- context scheduling (moved out of the kernel) ---

    /// Context primitives: the actor decides when they run.
    pub async fn context_ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.context.ingest(ingress).await
    }

    pub async fn context_maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.context.maintain(trigger).await
    }

    /// Run a full GC pass (mark roots, sweep, reversible eviction). Called
    /// by the actor at turn boundaries; engines without a GC pass return an
    /// empty report.
    pub async fn context_gc(&self) -> AgentResult<ContextGcReport> {
        self.context.gc().await
    }

    /// Materialize the working set for one model request. The result is
    /// structured items; prompt assembly happens in the runtime actor.
    pub async fn context_materialize(
        &self,
        query: ContextQuery,
    ) -> AgentResult<MaterializedContext> {
        self.context.materialize(query).await
    }

    /// Open a scope (runtime-driven, e.g. a tool scope at tool start).
    pub async fn context_open_scope(
        &self,
        kind: ScopeKind,
        parent: Option<ScopeId>,
    ) -> AgentResult<ScopeId> {
        self.context.open_scope(kind, parent).await
    }

    /// Close a scope the runtime opened; returns the close transitions.
    pub async fn context_close_scope(
        &self,
        scope_id: ScopeId,
    ) -> AgentResult<Vec<ContextStateTransition>> {
        self.context.close_scope(scope_id).await
    }

    pub async fn inspect_context(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.context.inspect(limit).await
    }

    /// Switch the runtime's focus to a task's goal. The task id comes from
    /// the runtime's `TaskManager` — re-focusing an existing task resumes
    /// its scopes in the context engine (suspension/resume is keyed on the
    /// task id), while a fresh task id opens a fresh task scope.
    pub async fn set_focus(
        &self,
        task_id: TaskId,
        goal: String,
    ) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let focus = FocusState::for_task(task_id, goal.clone());
        let transition = async {
            self.context
                .ingest(ContextIngress::FocusChanged { focus })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("set focus", checkpoint, transition)
            .await
    }

    /// Suspend the current focus without completing the task: the engine
    /// clears its focus and suspends the active task's scopes, so a later
    /// `set_focus` with the same task id resumes them.
    pub async fn clear_focus(&self) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context.ingest(ContextIngress::FocusCleared).await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("clear focus", checkpoint, transition)
            .await
    }

    pub async fn pin(&self, content: String) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context
                .ingest(ContextIngress::Pin {
                    content,
                    kind: agent_contracts::ContextKind::Constraint,
                })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::FocusChanged)
                .await
        }
        .await;
        self.finish_context_transaction("pin context", checkpoint, transition)
            .await
    }

    pub async fn complete_current_task(
        &self,
        task_id: TaskId,
        summary: String,
    ) -> AgentResult<ContextMaintenanceReport> {
        let checkpoint = self.context.checkpoint().await?;
        let transition = async {
            self.context
                .ingest(ContextIngress::TaskCompleted {
                    task_id: Some(task_id),
                    summary,
                })
                .await?;
            self.context
                .maintain(ContextMaintenanceTrigger::TaskCompleted)
                .await
        }
        .await;
        self.finish_context_transaction("complete task", checkpoint, transition)
            .await
    }

    /// Complete a context-only transaction. Context engines are replaceable
    /// and their mutation methods are fallible, so the runtime takes a
    /// portable checkpoint before a multi-step transition and restores it
    /// if either ingest or maintenance fails. Task state is committed by
    /// the runtime actor only after this method returns `Ok`.
    async fn finish_context_transaction<T>(
        &self,
        operation: &'static str,
        checkpoint: serde_json::Value,
        result: AgentResult<T>,
    ) -> AgentResult<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => match self.context.restore(checkpoint).await {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AgentError::RecoveryRequired(format!(
                    "{operation} failed ({error}); rollback failed ({rollback_error})"
                ))),
            },
        }
    }

    // --- tool lifecycle and surface scheduling (moved out of the kernel) ---

    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.specs()
    }

    pub fn tool_snapshot(&self) -> ToolSurfaceSnapshot {
        self.tools.snapshot()
    }

    pub fn tool_may_omit_from_round(&self, name: &str) -> bool {
        self.tools.may_omit_from_round(name)
    }

    /// Run the tool lifecycle safe point for one model round. `roots`
    /// names the active task's tool-demand set: those tools are never aged
    /// out by idle GC (TaskAnchor-driven tool roots), so a task that
    /// requires a tool keeps it available across rounds.
    pub fn tool_gc(&self, roots: &[String]) {
        self.tools.gc(roots);
    }

    pub fn tool_catalog(&self) -> Vec<ToolCatalogEntry> {
        self.tools.catalog()
    }

    pub fn tool_load(&self, name: &str) -> AgentResult<()> {
        self.tools.load_tool(name)
    }

    pub fn tool_unload(&self, name: &str) -> AgentResult<()> {
        self.tools.unload_tool(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ContextDiagnostics, ModelCapabilities, ToolExecutionRequest, ToolOutcome,
    };
    use agent_core::PolicyApprovalGate;

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
    fn services_share_one_kernel_and_round_trip_the_registry() {
        let services = RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(StubContext),
            Arc::new(StubModel),
            Arc::new(StubTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        );
        // The kernel is derived once: two `kernel()` calls share one
        // authority instance (same run id), so a subscriber on one sees
        // the other's events.
        let kernel = services.kernel();
        assert_eq!(kernel.run_id(), services.kernel().run_id());
        assert_eq!(
            services.system_prompt(),
            services.kernel_config.system_prompt
        );
        assert_eq!(
            services.context_budget_tokens(),
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
        let resolved = RuntimeServices::from_registry(&registry, CoreAuthorityConfig::default())
            .expect("every required service is present");
        assert_eq!(
            resolved.system_prompt(),
            services.system_prompt(),
            "from_registry preserves the root's configuration"
        );
    }
}

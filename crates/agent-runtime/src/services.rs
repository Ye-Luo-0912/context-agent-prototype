//! The composition seam (MOD-04A): the concrete implementations one run
//! needs, resolved from the module host's typed registry or built directly
//! in tests. A composition root constructs one `RuntimeServices` and hands
//! it to the runtime; the runtime uses it for *all* scheduling — context
//! maintenance and focus transactions, model calls, tool lifecycle and
//! surface scheduling, config access — while the CorePort it derives from
//! the services stays authority-only (events, approval, effects, output, and
//! the tool-execution wiring that combines them). The concrete Core stays
//! private to `agent-core`; it never constructs or schedules Runtime services.

use std::sync::Arc;

use agent_contracts::{
    AgentError, AgentResult, ApprovalGate, ContextEngine, ContextGcReport, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, EffectReconciler, EventJournal, FocusState, FsRereadClass,
    MaterializedContext, ModelCapabilities, ModelEventSink, ModelOutput, ModelRequest,
    ModelTransport, ScopeId, ScopeKind, StorageGcReport, TaskId, ToolCall, ToolCatalogEntry,
    ToolDispatcher, ToolExecutionAttribution, ToolLeaseReconcileReport, ToolSpec,
    ToolSurfaceSnapshot,
};
use agent_core::{CoreAuthorityConfig, CorePort, build_core_port, try_build_core_port};
use agent_workspace::Workspace;

use crate::host::ServiceRegistry;

/// The concrete implementations one run needs, plus the CorePort derived from
/// them. Scheduling — context maintenance, model calls, tool lifecycle —
/// lives here in the runtime; CorePort is the authority seam
/// (`services.core_port()`) the actor consults for events, approval, effects,
/// output and tool-execution wiring.
pub struct RuntimeServices {
    core: Arc<dyn CorePort>,
    kernel_config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    model: Arc<dyn ModelTransport>,
    tools: Arc<dyn ToolDispatcher>,
    /// Optional artifact destination (the run's workspace). When set, the
    /// actor persists each final assistant response in full before the
    /// bounded ContextItem is built, so the raw output survives ContextItem
    /// truncation (raw-evidence retention). `None` skips the persistence
    /// (tests and bare compositions).
    artifact_workspace: Option<Arc<Workspace>>,
    /// Ablation: when false, PromptAssembler omits TaskProgress. Default true.
    project_task_progress: bool,
}

/// Trusted, construction-time recovery dependencies for Core authority.
/// Grouping these prevents the normal scheduling service constructor from
/// growing one parameter per recovery adapter.
pub struct AuthorityRecoveryServices {
    operation_journal: Arc<dyn agent_contracts::OperationJournal>,
    effect_reconciler: Option<Arc<dyn EffectReconciler>>,
}

impl AuthorityRecoveryServices {
    pub fn new(
        operation_journal: Arc<dyn agent_contracts::OperationJournal>,
        effect_reconciler: Option<Arc<dyn EffectReconciler>>,
    ) -> Self {
        Self {
            operation_journal,
            effect_reconciler,
        }
    }
}

impl RuntimeServices {
    /// Build services directly (tests, standalone composition roots). The
    /// CorePort is derived once here, so the actor and handle share one
    /// authority instance (same run id, sequence and
    /// event channel). The model transport is a *scheduling* service: the
    /// Core authority does not call the provider, so it is not part of
    /// Core's inputs.
    pub fn new(
        kernel_config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn ApprovalGate>,
        journal: Option<Arc<dyn EventJournal>>,
    ) -> Self {
        let core = build_core_port(
            kernel_config.clone(),
            context.clone(),
            tools.clone(),
            approval,
            journal,
        );
        Self {
            core,
            kernel_config,
            context,
            model,
            tools,
            artifact_workspace: None,
            project_task_progress: true,
        }
    }

    /// Fallible construction for a Core configured with recoverable
    /// operation authority. Journal recovery and the startup epoch fence
    /// complete before the services become visible to Runtime.
    pub fn try_new(
        kernel_config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        approval: Arc<dyn ApprovalGate>,
        journal: Option<Arc<dyn EventJournal>>,
        authority_recovery: AuthorityRecoveryServices,
    ) -> AgentResult<Self> {
        let core = try_build_core_port(
            kernel_config.clone(),
            context.clone(),
            tools.clone(),
            approval,
            journal,
            Some(authority_recovery.operation_journal),
            authority_recovery.effect_reconciler,
        )?;
        Ok(Self {
            core,
            kernel_config,
            context,
            model,
            tools,
            artifact_workspace: None,
            project_task_progress: true,
        })
    }

    /// Resolve every service from the module host's typed registry. The
    /// kernel configuration stays with the composition root (the broker,
    /// shadow gate and lease TTL are root decisions, not registry
    /// services).
    pub fn from_registry(
        registry: &ServiceRegistry,
        kernel_config: CoreAuthorityConfig,
    ) -> AgentResult<Self> {
        let mut services = Self::new(
            kernel_config,
            registry.context_service()?,
            registry.model_provider()?,
            registry.tool_provider()?,
            registry.approval_policy()?,
            registry.event_store()?,
        );
        // Raw-evidence retention destination: the run's artifact store,
        // when the composition root wired one.
        services.artifact_workspace = registry.artifact_store()?;
        Ok(services)
    }

    /// Resolve services while installing a recoverable Core authority WAL.
    /// Recovery and the startup epoch fence must complete before Runtime is
    /// exposed, so this path is explicitly fallible.
    pub fn from_registry_with_operation_journal(
        registry: &ServiceRegistry,
        kernel_config: CoreAuthorityConfig,
        authority_recovery: AuthorityRecoveryServices,
    ) -> AgentResult<Self> {
        let mut services = Self::try_new(
            kernel_config,
            registry.context_service()?,
            registry.model_provider()?,
            registry.tool_provider()?,
            registry.approval_policy()?,
            registry.event_store()?,
            authority_recovery,
        )?;
        services.artifact_workspace = registry.artifact_store()?;
        Ok(services)
    }

    /// Attach the workspace used for exact assistant-response artifacts.
    ///
    /// This consuming builder is intended for trusted, direct composition
    /// roots that do not use [`Self::from_registry`]. Runtime consumers do
    /// not receive the workspace handle back; only the actor's bounded
    /// artifact-write path may access it.
    pub fn with_artifact_workspace(mut self, workspace: Arc<Workspace>) -> Self {
        self.artifact_workspace = Some(workspace);
        self
    }

    pub fn with_project_task_progress(mut self, project: bool) -> Self {
        self.project_task_progress = project;
        self
    }

    pub(crate) fn project_task_progress(&self) -> bool {
        self.project_task_progress
    }

    pub(crate) fn artifact_workspace(&self) -> Option<&Workspace> {
        self.artifact_workspace.as_deref()
    }

    /// Narrow authority port shared by the actor and spawn seam. It exposes
    /// no concrete Core implementation or component-authority handles.
    pub(crate) fn core_port(&self) -> Arc<dyn CorePort> {
        self.core.clone()
    }

    // --- configuration (moved out of the kernel) ---

    pub(crate) fn system_prompt(&self) -> String {
        self.kernel_config.system_prompt.clone()
    }

    pub(crate) fn context_budget_tokens(&self) -> usize {
        self.kernel_config.context_budget_tokens
    }

    pub(crate) fn max_tool_rounds(&self) -> usize {
        self.kernel_config.max_tool_rounds
    }

    // --- model scheduling (moved out of the kernel) ---

    pub(crate) fn model_capabilities(&self) -> ModelCapabilities {
        self.model.capabilities()
    }

    /// One model round: stream the request to the provider. The result is a
    /// value for the actor to validate and commit — nothing is committed
    /// here.
    pub(crate) async fn run_model_round(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        self.model.complete_stream(request, sink).await
    }

    // --- context scheduling (moved out of the kernel) ---

    /// Context primitives: the actor decides when they run.
    pub(crate) async fn context_ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.context.ingest(ingress).await
    }

    pub(crate) async fn context_fs_read_residency(&self, path: &str) -> AgentResult<FsRereadClass> {
        self.context.fs_read_residency(path).await
    }

    pub(crate) async fn context_maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.context.maintain(trigger).await
    }

    /// Run a full GC pass (mark roots, sweep, reversible eviction). Called
    /// by the actor at turn boundaries; engines without a GC pass return an
    /// empty report.
    pub(crate) async fn context_gc(&self) -> AgentResult<ContextGcReport> {
        self.context.gc().await
    }

    /// Run one conservative Storage GC pass (the only place information is
    /// permanently deleted). The runtime schedules it only at explicit
    /// boundaries — task completion, checkpoint — never on the per-model
    /// hot path.
    pub(crate) async fn context_storage_gc(&self) -> AgentResult<StorageGcReport> {
        self.context.storage_gc().await
    }

    /// Materialize the working set for one model request. The result is
    /// structured items; prompt assembly happens in the runtime actor.
    pub(crate) async fn context_materialize(
        &self,
        query: ContextQuery,
    ) -> AgentResult<MaterializedContext> {
        self.context.materialize(query).await
    }

    /// Open a scope (runtime-driven, e.g. a tool scope at tool start).
    pub(crate) async fn context_open_scope(
        &self,
        kind: ScopeKind,
        parent: Option<ScopeId>,
    ) -> AgentResult<ScopeId> {
        self.context.open_scope(kind, parent).await
    }

    /// Close a scope the runtime opened; returns the close transitions.
    pub(crate) async fn context_close_scope(
        &self,
        scope_id: ScopeId,
    ) -> AgentResult<Vec<ContextStateTransition>> {
        self.context.close_scope(scope_id).await
    }

    pub(crate) async fn inspect_context(
        &self,
        limit: usize,
    ) -> AgentResult<Vec<ContextItemSummary>> {
        self.context.inspect(limit).await
    }

    /// Switch the runtime's focus to a task's goal. The task id comes from
    /// the runtime's `TaskManager` — re-focusing an existing task resumes
    /// its scopes in the context engine (suspension/resume is keyed on the
    /// task id), while a fresh task id opens a fresh task scope.
    pub(crate) async fn set_focus(
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
    pub(crate) async fn clear_focus(&self) -> AgentResult<ContextMaintenanceReport> {
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

    pub(crate) async fn pin(&self, content: String) -> AgentResult<ContextMaintenanceReport> {
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

    pub(crate) async fn complete_current_task(
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

    pub(crate) fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.specs()
    }

    pub(crate) fn tool_snapshot(&self) -> ToolSurfaceSnapshot {
        self.tools.snapshot()
    }

    pub(crate) fn tool_may_omit_from_round(&self, name: &str) -> bool {
        self.tools.may_omit_from_round(name)
    }

    /// Run the tool lifecycle safe point for one model round. `roots`
    /// names the active task's tool-demand set: those tools are never aged
    /// out by idle GC (TaskAnchor-driven tool roots), so a task that
    /// requires a tool keeps it available across rounds.
    pub(crate) fn tool_gc(&self, roots: &[String]) {
        self.tools.gc(roots);
    }

    /// Project runtime-owned leases onto the mutable schema surface at an
    /// actor safe point. This is separate from idle/pressure GC: it advances
    /// no clock and releases only optional schemas without a current source.
    pub(crate) fn tool_reconcile_leases(&self, roots: &[String]) -> ToolLeaseReconcileReport {
        self.tools.reconcile_leases(roots)
    }

    pub(crate) fn tool_catalog(&self) -> Vec<ToolCatalogEntry> {
        self.tools.catalog()
    }

    pub(crate) fn tool_execution_attribution(&self, call: &ToolCall) -> ToolExecutionAttribution {
        self.tools.execution_attribution(call)
    }

    pub(crate) fn tool_load_for_lease(&self, name: &str) -> AgentResult<()> {
        self.tools.load_tool_for_lease(name)
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
                task: None,
                items: Vec::new(),
                external: agent_contracts::ContextMapView::default(),
                selected: Vec::new(),
                approx_tokens: 0,
                foreground: Vec::new(),
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
        let config = CoreAuthorityConfig::default();
        let expected_system_prompt = config.system_prompt.clone();
        let expected_context_budget_tokens = config.context_budget_tokens;
        let context: Arc<dyn ContextEngine> = Arc::new(StubContext);
        let model: Arc<dyn ModelTransport> = Arc::new(StubModel);
        let tools: Arc<dyn ToolDispatcher> = Arc::new(StubTools);
        let approval: Arc<dyn ApprovalGate> = Arc::new(PolicyApprovalGate::read_only());
        let services = RuntimeServices::new(
            config,
            context.clone(),
            model.clone(),
            tools.clone(),
            approval.clone(),
            None,
        );
        // The Core port is derived once: two clones share one
        // authority instance (same run id), so a subscriber on one sees
        // the other's events.
        let core = services.core_port();
        assert_eq!(core.run_id(), services.core_port().run_id());
        assert_eq!(services.system_prompt(), expected_system_prompt);
        assert_eq!(
            services.context_budget_tokens(),
            expected_context_budget_tokens
        );

        // A registry that publishes the same services resolves them back.
        let mut registry = ServiceRegistry::new();
        registry
            .register(crate::host::CONTEXT_SERVICE, "test", context)
            .unwrap();
        registry
            .register(crate::host::MODEL_PROVIDER, "test", model)
            .unwrap();
        registry
            .register(crate::host::TOOL_PROVIDER, "test", tools)
            .unwrap();
        registry
            .register(crate::host::APPROVAL_POLICY, "test", approval)
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

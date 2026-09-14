//! The composition seam: the concrete implementations one run
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
    AgentError, AgentResult, ApprovalGate, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextStateTransition, EffectReconciler, EventJournal, FocusState, FsRereadClass,
    ModelCapabilities, ModelTransport, PromptLayout, ScopeId, ScopeKind, StorageGcReport,
    StoreReconcileReport, TaskId, ToolCall, ToolCatalogEntry, ToolDispatcher,
    ToolExecutionAttribution, ToolLeaseReconcileReport, ToolSpec, ToolSurfaceSnapshot,
    VerificationCoverageDeclaration,
};
use agent_core::{CoreAuthorityConfig, CorePort, build_core_port, try_build_core_port};
use agent_workspace::Workspace;

use crate::host::ServiceRegistry;

/// The concrete implementations one run needs, plus the CorePort derived from
/// them. Scheduling — context maintenance, model calls, tool lifecycle —
/// lives here in the runtime; CorePort is the authority seam
/// (`services.core_port()`) the actor consults for events, approval, effects,
/// output and tool-execution wiring.
/// The fields are grouped in construction order: the product configuration
/// (services, durable plumbing and product behavior switches) first, then
/// the frozen experiment-projection switches. The two groups are also the
/// two constructor bundles ([`ProductServicesConfig`] and
/// [`ExperimentProjection`]); `new` and `try_new` converge on
/// [`RuntimeServices::from_parts`] so there is exactly one initialization
/// path for both.
pub struct RuntimeServices {
    // --- product configuration ---
    core: Arc<dyn CorePort>,
    kernel_config: CoreAuthorityConfig,
    context: Arc<dyn ContextEngine>,
    model: Arc<dyn ModelTransport>,
    tools: Arc<dyn ToolDispatcher>,
    /// EXEC-8 (R2-09): the durable event journal, retained for bounded
    /// read-back (cold completion lookups). `None` = this composition has
    /// no journal and lookups answer beyond-window honestly.
    event_journal: Option<Arc<dyn EventJournal>>,
    /// Immutable, bounded construction-time projection of the concrete
    /// host's coverage table. Completion reads this narrow contract rather
    /// than reaching into a tool implementation, and re-composition takes a
    /// fresh snapshot so persisted receipts cannot follow an upgraded table.
    verification_coverage_declarations: Arc<[VerificationCoverageDeclaration]>,
    /// Optional artifact destination (the run's workspace). When set, the
    /// actor persists each final assistant response in full before the
    /// bounded ContextItem is built, so the raw output survives ContextItem
    /// truncation (raw-evidence retention). `None` skips the persistence
    /// (tests and bare compositions).
    artifact_workspace: Option<Arc<Workspace>>,
    /// Key-free serving identity from the composition root, persisted into
    /// every checkpoint's run metadata. Absent when the composition did
    /// not provide one.
    provider_profile_digest: Option<String>,
    /// N04: the composition root's stable cache-routing namespace. When
    /// present, the actor stamps every main-lane model request with the
    /// derived per-task key; `None` keeps requests keyless (unknown-
    /// capability endpoints and bare compositions keep the historical
    /// payload).
    cache_routing: Option<agent_contracts::PromptCacheRouting>,
    /// Read-only handle onto the host capability registry, injected at
    /// spawn so the actor's safe-point checkpoints capture the full plane
    /// set. The actor snapshots it; it never mutates through this handle.
    capability_registry: Option<Arc<crate::capability::CapabilityRegistry>>,
    /// Optional host-side exact verifier for the completion-gate
    /// proof-refresh transaction. `None` keeps ordinary refusals.
    proof_verifier: Option<Arc<dyn crate::verification::ProofVerifier>>,
    /// Product opt-in: when false (the default), a completion-time proof
    /// refresh runs inline in the actor, preserving the historical
    /// same-round gate result. When true, the host verifier runs outside
    /// the actor loop and the parked completion resumes when it finishes.
    defer_proof_refresh: bool,
    /// Product opt-in: compile the shadow Context Frame manifest and emit
    /// it as `ContextFrameShadow` diagnostics. Never changes model input.
    shadow_context_frame: bool,
    /// Product surface choice: message placement only; Context selection
    /// and focus policy are unchanged.
    prompt_layout: PromptLayout,
    /// Product gate (default off, promotion-gated): when false the gate
    /// never runs the proof-refresh transaction even with a verifier
    /// injected.
    project_proof_refresh: bool,
    // --- frozen experiment projection (ablation switches; never deleted) ---
    /// Ablation: when false, PromptAssembler omits TaskProgress. Default true.
    project_task_progress: bool,
    /// Ablation: when true, a settled-candidate fact is projected into the
    /// otherwise unchanged TaskProgress view. Kept separate from
    /// `project_task_progress` so paired evaluation arms do not remove the
    /// runtime's progress memory as a confound.
    project_settlement: bool,
    /// Expensive causal-evaluation diagnostics. When enabled, both
    /// settlement arms pack against the treatment-sized envelope and the
    /// runtime emits a same-state counterfactual digest in request metadata.
    /// Product runs keep this false: an off candidate must not pay for or
    /// execute experiment-only packing, cloning, assembly, or hashing.
    settlement_projection_diagnostics: bool,
    /// Completion-opportunity advisory: when false (the default), the
    /// runtime never derives completion-opportunity facts, never leases
    /// `task.complete` from derived readiness and emits no opportunity
    /// events.
    project_completion_opportunity: bool,
    /// Directory-tool admission candidate: when false (the default), a
    /// typed missing-parent refusal never changes the model surface, so
    /// `fs.mkdir` stays catalog-cold exactly as the baseline. When true,
    /// the trusted recovery source surfaces the exact host-owned tool for
    /// one decision. Promotion requires the isolation paired live gate.
    recovery_surface: bool,
}

/// The product-configuration half of a [`RuntimeServices`] construction:
/// the durable plumbing a composition root resolves plus the
/// product-visible behavior switches. Grouping these apart from
/// [`ExperimentProjection`] states in one place which switches a product
/// entry point sets; the defaults here are the product baseline every
/// constructor starts from.
struct ProductServicesConfig {
    event_journal: Option<Arc<dyn EventJournal>>,
    verification_coverage_declarations: Arc<[VerificationCoverageDeclaration]>,
    artifact_workspace: Option<Arc<Workspace>>,
    provider_profile_digest: Option<String>,
    cache_routing: Option<agent_contracts::PromptCacheRouting>,
    capability_registry: Option<Arc<crate::capability::CapabilityRegistry>>,
    proof_verifier: Option<Arc<dyn crate::verification::ProofVerifier>>,
    defer_proof_refresh: bool,
    shadow_context_frame: bool,
    prompt_layout: PromptLayout,
    project_proof_refresh: bool,
}

impl Default for ProductServicesConfig {
    fn default() -> Self {
        Self {
            event_journal: None,
            verification_coverage_declarations: Arc::from([]),
            artifact_workspace: None,
            provider_profile_digest: None,
            cache_routing: None,
            capability_registry: None,
            proof_verifier: None,
            defer_proof_refresh: false,
            shadow_context_frame: false,
            prompt_layout: PromptLayout::CurrentStateLast,
            project_proof_refresh: false,
        }
    }
}

/// The frozen experiment-projection switches of a [`RuntimeServices`]
/// construction. These are ablation/projection bools with paired-evaluation
/// semantics; they are grouped (not removed) so a product baseline and an
/// experiment arm differ by exactly one explicit switch. The baseline is
/// the product configuration: TaskProgress on, every projection off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExperimentProjection {
    project_task_progress: bool,
    project_settlement: bool,
    settlement_projection_diagnostics: bool,
    project_completion_opportunity: bool,
    recovery_surface: bool,
}

impl Default for ExperimentProjection {
    fn default() -> Self {
        Self::baseline()
    }
}

impl ExperimentProjection {
    /// The frozen product baseline every composition starts from.
    const fn baseline() -> Self {
        Self {
            project_task_progress: true,
            project_settlement: false,
            settlement_projection_diagnostics: false,
            project_completion_opportunity: false,
            recovery_surface: false,
        }
    }
}

fn snapshot_verification_coverage_declarations(
    tools: &dyn ToolDispatcher,
) -> Arc<[VerificationCoverageDeclaration]> {
    let declarations = tools.verification_coverage_declarations();
    let canonical = declarations.len() <= agent_contracts::MAX_VERIFICATION_COVERAGE_DECLARATIONS
        && declarations
            .iter()
            .all(VerificationCoverageDeclaration::is_valid)
        && declarations
            .windows(2)
            .all(|pair| pair[0].domain_id < pair[1].domain_id);
    if canonical {
        declarations.into()
    } else {
        // A partially usable host table is more dangerous than no table:
        // keep the run operational but make every evidence closure fail.
        Arc::from([])
    }
}

/// A post-completion context plane that is live but not yet authorized by
/// the terminal runtime checkpoint. RuntimeActor either consumes its report
/// after that durable freeze or restores `before`; no other component can
/// commit the cross-plane transaction.
pub(crate) struct PreparedTaskCompletion {
    before: serde_json::Value,
    pub(crate) report: ContextMaintenanceReport,
}

impl RuntimeServices {
    /// The single initialization path [`Self::new`] and [`Self::try_new`]
    /// converge on: the two constructors differ only in how the Core port
    /// is built (plain vs recoverable authority); everything else — product
    /// configuration and frozen experiment projection — is assembled here
    /// exactly once, so the two paths cannot drift apart.
    fn from_parts(
        core: Arc<dyn CorePort>,
        kernel_config: CoreAuthorityConfig,
        context: Arc<dyn ContextEngine>,
        model: Arc<dyn ModelTransport>,
        tools: Arc<dyn ToolDispatcher>,
        product: ProductServicesConfig,
        experiments: ExperimentProjection,
    ) -> Self {
        let ProductServicesConfig {
            event_journal,
            verification_coverage_declarations,
            artifact_workspace,
            provider_profile_digest,
            cache_routing,
            capability_registry,
            proof_verifier,
            defer_proof_refresh,
            shadow_context_frame,
            prompt_layout,
            project_proof_refresh,
        } = product;
        let ExperimentProjection {
            project_task_progress,
            project_settlement,
            settlement_projection_diagnostics,
            project_completion_opportunity,
            recovery_surface,
        } = experiments;
        Self {
            core,
            kernel_config,
            context,
            model,
            tools,
            event_journal,
            verification_coverage_declarations,
            artifact_workspace,
            provider_profile_digest,
            cache_routing,
            capability_registry,
            proof_verifier,
            defer_proof_refresh,
            shadow_context_frame,
            prompt_layout,
            project_proof_refresh,
            project_task_progress,
            project_settlement,
            settlement_projection_diagnostics,
            project_completion_opportunity,
            recovery_surface,
        }
    }

    /// Live registry handle for the capture-side generation handshake.
    pub(crate) fn capability_registry(
        &self,
    ) -> Option<&Arc<crate::capability::CapabilityRegistry>> {
        self.capability_registry.as_ref()
    }

    /// Spawn-time injection probe and setter (crate-private).
    pub(crate) fn capability_snapshot_for_spawn(
        &self,
    ) -> Option<&Arc<crate::capability::CapabilityRegistry>> {
        self.capability_registry.as_ref()
    }

    pub(crate) fn set_capability_registry(
        &mut self,
        registry: Arc<crate::capability::CapabilityRegistry>,
    ) {
        self.capability_registry = Some(registry);
    }
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
        let verification_coverage_declarations =
            snapshot_verification_coverage_declarations(tools.as_ref());
        let core = build_core_port(
            kernel_config.clone(),
            context.clone(),
            tools.clone(),
            approval,
            journal.clone(),
        );
        Self::from_parts(
            core,
            kernel_config,
            context,
            model,
            tools,
            ProductServicesConfig {
                event_journal: journal,
                verification_coverage_declarations,
                ..ProductServicesConfig::default()
            },
            ExperimentProjection::default(),
        )
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
        let verification_coverage_declarations =
            snapshot_verification_coverage_declarations(tools.as_ref());
        let core = try_build_core_port(
            kernel_config.clone(),
            context.clone(),
            tools.clone(),
            approval,
            journal.clone(),
            Some(authority_recovery.operation_journal),
            authority_recovery.effect_reconciler,
        )?;
        Ok(Self::from_parts(
            core,
            kernel_config,
            context,
            model,
            tools,
            ProductServicesConfig {
                event_journal: journal,
                verification_coverage_declarations,
                ..ProductServicesConfig::default()
            },
            ExperimentProjection::default(),
        ))
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

    /// Composition-root-provided key-free serving identity, persisted with
    /// every checkpoint's run metadata.
    pub fn set_provider_profile_digest(&mut self, digest: String) {
        self.provider_profile_digest = Some(digest);
    }

    /// N04: inject the stable cache-routing namespace (composition root).
    pub fn set_cache_routing(&mut self, routing: agent_contracts::PromptCacheRouting) {
        self.cache_routing = Some(routing);
    }

    /// N04: the routing namespace for the main model lane, if configured.
    pub fn cache_routing(&self) -> Option<&agent_contracts::PromptCacheRouting> {
        self.cache_routing.as_ref()
    }

    pub fn provider_profile_digest(&self) -> Option<&str> {
        self.provider_profile_digest.as_deref()
    }

    /// Opt-in deferral of completion-time proof refresh (see the field
    /// doc).
    pub fn defer_proof_refresh(&self) -> bool {
        self.defer_proof_refresh
    }

    pub fn with_defer_proof_refresh(mut self, defer: bool) -> Self {
        self.defer_proof_refresh = defer;
        self
    }

    /// Opt-in shadow Context Frame manifest emission (see the field doc).
    pub fn shadow_context_frame(&self) -> bool {
        self.shadow_context_frame
    }

    pub fn with_shadow_context_frame(mut self, shadow: bool) -> Self {
        self.shadow_context_frame = shadow;
        self
    }

    pub fn with_project_task_progress(mut self, project: bool) -> Self {
        self.project_task_progress = project;
        self
    }

    pub fn with_prompt_layout(mut self, layout: PromptLayout) -> Self {
        self.prompt_layout = layout;
        self
    }

    pub fn prompt_layout(&self) -> PromptLayout {
        self.prompt_layout
    }

    pub(crate) fn project_task_progress(&self) -> bool {
        self.project_task_progress
    }

    /// Opt the runtime into projecting the neutral settled-candidate fact.
    /// TaskProgress itself stays independently enabled so an off/on pair
    /// changes only this one fact.
    pub fn with_project_settlement(mut self, project: bool) -> Self {
        self.project_settlement = project;
        self
    }

    pub(crate) fn project_settlement(&self) -> bool {
        self.project_settlement
    }

    /// Enable the expensive same-state settlement comparison used by the
    /// paired causal harness. This is deliberately independent of the arm:
    /// a valid pair enables it identically in both cells.
    pub fn with_settlement_projection_diagnostics(mut self, enabled: bool) -> Self {
        self.settlement_projection_diagnostics = enabled;
        self
    }

    pub(crate) fn settlement_projection_diagnostics(&self) -> bool {
        self.settlement_projection_diagnostics
    }

    /// Opt the runtime into deriving advisory completion-opportunity facts.
    /// Default off; promotion requires the ROADMAP item-8 off/on paired
    /// live gate before this may ship enabled.
    pub fn with_project_completion_opportunity(mut self, project: bool) -> Self {
        self.project_completion_opportunity = project;
        self
    }

    pub(crate) fn project_completion_opportunity(&self) -> bool {
        self.project_completion_opportunity
    }

    /// Opt the runtime into the trusted recovery surface for directory
    /// topology. Default off: `fs.mkdir` stays catalog-cold exactly as the
    /// baseline until the isolated paired live gate promotes it. The switch
    /// is the only variable between the two gate arms.
    pub fn with_recovery_surface(mut self, enabled: bool) -> Self {
        self.recovery_surface = enabled;
        self
    }

    pub(crate) fn recovery_surface(&self) -> bool {
        self.recovery_surface
    }

    /// Inject the host-side exact verifier for the completion-gate
    /// proof-refresh transaction. Without it (the default) the gate always
    /// returns ordinary refusals even when the switch is on.
    pub fn with_proof_verifier(
        mut self,
        verifier: Arc<dyn crate::verification::ProofVerifier>,
    ) -> Self {
        self.proof_verifier = Some(verifier);
        self
    }

    /// Opt the gate into the runtime-owned proof-refresh transaction.
    /// Default off; only a consciously injected host verifier may enable it.
    pub fn with_project_proof_refresh(mut self, project: bool) -> Self {
        self.project_proof_refresh = project;
        self
    }

    pub(crate) fn proof_verifier(&self) -> Option<&Arc<dyn crate::verification::ProofVerifier>> {
        self.proof_verifier.as_ref()
    }

    pub(crate) fn project_proof_refresh(&self) -> bool {
        self.project_proof_refresh
    }

    pub(crate) fn artifact_workspace(&self) -> Option<&Workspace> {
        self.artifact_workspace.as_deref()
    }

    /// The tool dispatcher lane, for typed execution facts at settlement
    /// time. Dispatch itself stays behind the Core port.
    pub(crate) fn tools(&self) -> Arc<dyn ToolDispatcher> {
        self.tools.clone()
    }

    pub(crate) fn verification_coverage_declarations(&self) -> &[VerificationCoverageDeclaration] {
        &self.verification_coverage_declarations
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

    /// Clone only the engine lane for a spawned maintenance operation.
    /// Engines serialize their own state; no tool/workspace authority moves
    /// into this task with the Arc.
    /// EXEC-8: bounded read-back access to the durable event journal.
    pub(crate) fn event_journal(&self) -> Option<Arc<dyn EventJournal>> {
        self.event_journal.clone()
    }

    pub(crate) fn context_engine(&self) -> Arc<dyn ContextEngine> {
        Arc::clone(&self.context)
    }

    /// Clone only the model scheduling lane for a detached request. A
    /// provider may take time to observe cancellation, so the detached task
    /// must not retain the complete service bundle or workspace authority.
    pub(crate) fn model_transport(&self) -> Arc<dyn ModelTransport> {
        self.model.clone()
    }

    // --- context scheduling (moved out of the kernel) ---

    /// Context primitives: the actor decides when they run.
    pub(crate) async fn context_ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.context.ingest(ingress).await
    }

    pub(crate) async fn context_fs_read_residency(&self, path: &str) -> AgentResult<FsRereadClass> {
        self.context.fs_read_residency(path).await
    }

    /// Capture the rollback basis before the actor dispatches both input
    /// ingestion and UserInput maintenance as one cancellable operation.
    pub(crate) async fn prepare_user_message(&self) -> AgentResult<serde_json::Value> {
        self.context.checkpoint().await
    }

    /// Commit a successful pass, or restore ingestion and maintenance
    /// together on failure/cancellation. Completion or a joined abort must
    /// first prove the engine future ended, so it cannot race the restore.
    pub(crate) async fn finish_user_message(
        &self,
        checkpoint: serde_json::Value,
        transition: AgentResult<ContextMaintenanceReport>,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.finish_context_transaction("apply user message", checkpoint, transition)
            .await
    }

    /// Run one conservative Storage GC pass (the only place information is
    /// permanently deleted). The runtime schedules it only at explicit
    /// boundaries — task completion, checkpoint — never on the per-model
    /// hot path.
    /// W03: Storage GC under the shared retained-root invariant — the
    /// same protected recovery roots reconcile honors also guard the
    /// completion-boundary deletion pass.
    pub(crate) async fn context_storage_gc_protecting(
        &self,
        protected_recovery_roots: &[ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<StorageGcReport> {
        self.context
            .storage_gc_protecting(protected_recovery_roots, roots_complete)
            .await
    }

    /// Reconcile the store while keeping `protected` ids' blobs alive:
    /// item ids still referenced by retained, restorable checkpoints must
    /// survive even when a newer snapshot made the id resident (R03).
    /// `roots_complete` is false when that enumeration failed — the same
    /// W03 invariant as Storage GC: an unknown owner defers deletion.
    pub(crate) async fn context_reconcile_store_protecting(
        &self,
        protected: &[ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<StoreReconcileReport> {
        self.context
            .reconcile_store_protecting(protected, roots_complete)
            .await
    }

    /// The external item ids one stored context checkpoint references —
    /// strong recovery roots a reconcile must not delete while the
    /// checkpoint is retained (R03).
    pub(crate) async fn context_checkpoint_recovery_item_ids(
        &self,
        checkpoint: &serde_json::Value,
    ) -> AgentResult<Vec<ContextItemId>> {
        self.context.checkpoint_recovery_item_ids(checkpoint).await
    }

    /// Materialize the working set for one model request. The result is
    /// structured items; prompt assembly happens in the runtime actor.
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

    pub(crate) async fn prepare_complete_current_task(
        &self,
        task_id: TaskId,
        summary: String,
    ) -> AgentResult<PreparedTaskCompletion> {
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
        match transition {
            Ok(report) => Ok(PreparedTaskCompletion {
                before: checkpoint,
                report,
            }),
            Err(error) => {
                self.finish_context_transaction("complete task", checkpoint, Err(error))
                    .await
            }
        }
    }

    /// Roll back a prepared terminal context plane when checkpoint assembly
    /// or persistence fails. Only rollback failure makes the planes
    /// unknowable and therefore requires the runtime recovery fence.
    pub(crate) async fn rollback_task_completion(
        &self,
        prepared: PreparedTaskCompletion,
    ) -> AgentResult<()> {
        self.context
            .restore(prepared.before)
            .await
            .map_err(|error| {
                AgentError::RecoveryRequired(format!(
                    "terminal task context rollback failed ({error})"
                ))
            })
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
    use agent_contracts::OperationJournal;
    use agent_contracts::{
        ContextDiagnostics, ContextQuery, MaterializedContext, ModelCapabilities, ModelOutput,
        ModelRequest, ToolExecutionRequest, ToolOutcome,
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
                required_item_ids: Vec::new(),
                required_misses: Default::default(),
                optional_misses: Default::default(),
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

    /// Minimal in-memory authority journal so `try_new` is exercisable in
    /// this module without a filesystem.
    #[derive(Default)]
    struct BaselineOperationJournal {
        sequence: std::sync::atomic::AtomicU64,
    }

    impl OperationJournal for BaselineOperationJournal {
        fn append_and_sync(
            &self,
            transition: &agent_contracts::OperationJournalTransition,
        ) -> AgentResult<agent_contracts::OperationJournalRecord> {
            Ok(agent_contracts::OperationJournalRecord {
                version: agent_contracts::OPERATION_JOURNAL_VERSION,
                seq: self
                    .sequence
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1,
                transition: transition.clone(),
            })
        }

        fn recover(&self) -> AgentResult<agent_contracts::OperationJournalRecovery> {
            Ok(agent_contracts::OperationJournalRecovery::default())
        }
    }

    /// T5 characterization: `new` and `try_new` converge on one
    /// initialization path and both produce the same frozen
    /// product/experiment baseline. If the grouped constructors ever drift
    /// a default, this test names the observable that moved.
    #[test]
    fn new_and_try_new_share_the_frozen_product_baseline() {
        let config = CoreAuthorityConfig::default();
        let context: Arc<dyn ContextEngine> = Arc::new(StubContext);
        let model: Arc<dyn ModelTransport> = Arc::new(StubModel);
        let tools: Arc<dyn ToolDispatcher> = Arc::new(StubTools);
        let approval: Arc<dyn ApprovalGate> = Arc::new(PolicyApprovalGate::read_only());
        let plain = RuntimeServices::new(
            config.clone(),
            context.clone(),
            model.clone(),
            tools.clone(),
            approval.clone(),
            None,
        );
        let recovered = RuntimeServices::try_new(
            config,
            context.clone(),
            model.clone(),
            tools.clone(),
            approval.clone(),
            None,
            AuthorityRecoveryServices::new(Arc::new(BaselineOperationJournal::default()), None),
        )
        .expect("the recoverable authority builds against an empty journal");
        for services in [&plain, &recovered] {
            // Product configuration defaults.
            assert!(services.artifact_workspace().is_none());
            assert!(services.provider_profile_digest().is_none());
            assert!(services.cache_routing().is_none());
            assert!(!services.defer_proof_refresh());
            assert!(!services.shadow_context_frame());
            assert_eq!(services.prompt_layout(), PromptLayout::CurrentStateLast);
            assert!(!services.project_proof_refresh());
            assert!(services.capability_registry().is_none());
            assert!(services.event_journal().is_none());
            // Frozen experiment projection baseline: TaskProgress on,
            // every projection/ablation switch off.
            assert!(services.project_task_progress());
            assert!(!services.project_settlement());
            assert!(!services.settlement_projection_diagnostics());
            assert!(!services.project_completion_opportunity());
            assert!(!services.recovery_surface());
            // A no-op tool surface snapshots to an empty coverage table.
            assert!(services.verification_coverage_declarations().is_empty());
        }
        // The grouped bundles themselves pin the frozen baseline.
        assert_eq!(
            ExperimentProjection::default(),
            ExperimentProjection::baseline()
        );
        assert!(ExperimentProjection::baseline().project_task_progress);
        assert!(!ExperimentProjection::baseline().project_settlement);
        assert!(!ExperimentProjection::baseline().settlement_projection_diagnostics);
        assert!(!ExperimentProjection::baseline().project_completion_opportunity);
        assert!(!ExperimentProjection::baseline().recovery_surface);
        assert_eq!(
            ProductServicesConfig::default().prompt_layout,
            PromptLayout::CurrentStateLast
        );
        assert!(!ProductServicesConfig::default().defer_proof_refresh);
        assert!(!ProductServicesConfig::default().shadow_context_frame);
        assert!(!ProductServicesConfig::default().project_proof_refresh);
    }
}

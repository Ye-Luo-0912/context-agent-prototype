use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use agent_contracts::{
    AgentResult, Capability, CapabilityInvocationContext, CapabilityLifecycle, CapabilityManifest,
    CapabilityOutcome, CapabilityStatus, CapabilityTransport, ContextAction, ContextDiagnostics,
    ContextEngine, ContextIngress, ContextItemSummary, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextStateTransition, EventJournal,
    MaterializedContext, ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput, ModelRequest,
    ModelTransport, RuntimeDirective, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind,
    ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    AuthorityRecoveryServices, CapabilityId, Module, ModuleHost, RuntimeInstance, RuntimeServices,
    ServiceRegistry,
};
use agent_storage::FileOperationJournal;

/// Build an instance over the real reference engine, so the checkpoint test
/// exercises items, scopes and focus — not a stub that trivially roundtrips.
pub(crate) async fn simple_instance() -> (RuntimeInstance, Arc<context_simple::SimpleContextEngine>)
{
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    (instance, context)
}

/// Build the same test composition with production-shaped durable Core
/// authority. Reopening the same journal after shutdown exercises a real
/// cross-process checkpoint lineage rather than weakening ephemeral rules.
pub(crate) async fn durable_simple_instance(
    journal_path: &Path,
) -> (RuntimeInstance, Arc<context_simple::SimpleContextEngine>) {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let operation_journal = Arc::new(FileOperationJournal::open(journal_path).unwrap().0);
    let services = RuntimeServices::try_new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
        AuthorityRecoveryServices::new(operation_journal, None),
    )
    .unwrap();
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    (instance, context)
}

#[derive(Debug, Default)]
pub(crate) struct TestContextEngine;

#[async_trait::async_trait]
impl ContextEngine for TestContextEngine {
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
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
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

/// A context engine that refuses the completion ingest, so the runtime's
/// completion transaction fails *before* the task authority plane commits.
/// Everything else behaves like the trivial `TestContextEngine`.
#[derive(Debug, Default)]
pub(crate) struct FailingCompleteEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingCompleteEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::TaskCompleted { .. }) {
            return Err(agent_contracts::AgentError::Internal(
                "simulated completion ingest failure".into(),
            ));
        }
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
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
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

pub(crate) struct CheckpointCapability {
    pub(crate) manifest: CapabilityManifest,
}

impl CheckpointCapability {
    pub(crate) fn new() -> Self {
        Self {
            manifest: CapabilityManifest {
                id: "checkpoint-capability".into(),
                version: "1.0.0".into(),
                name: "checkpoint capability".into(),
                summary: "tests restore ordering".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![agent_contracts::CapabilityKind::Tool],
                permissions: Vec::new(),
                requires: Vec::new(),
                tools: Vec::new(),
                lifecycle: CapabilityLifecycle::Lazy,
                transport: CapabilityTransport::Builtin,
                sandbox_profile: Default::default(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Capability for CheckpointCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "checkpoint.tool".into(),
            description: "checkpoint test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }

    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        Ok(CapabilityOutcome::Value(ToolOutput {
            call_id: call.id,
            tool_name: call.name,
            ok: true,
            summary: "ok".into(),
            model_content: "ok".into(),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        }))
    }
}

#[derive(Debug)]
pub(crate) struct EmptyTools;

#[async_trait::async_trait]
impl ToolDispatcher for EmptyTools {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Err(agent_contracts::AgentError::Tool(
            "no tools configured".into(),
        ))
    }
}

/// A dispatcher that answers one `context.manage` admit call with the
/// typed runtime directive, so the full tool -> runtime -> engine admit
/// route is exercised without the real builtin surface.
#[derive(Debug)]
pub(crate) struct AdmitDirectiveDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for AdmitDirectiveDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: agent_contracts::CONTEXT_MANAGE.into(),
            description: "admit a ref back into the working set".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let item_id = request
            .call
            .arguments
            .get("item_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| agent_contracts::AgentError::Tool("missing item_id".into()))?
            .to_string();
        let output = ToolOutput {
            call_id: request.call.id.clone(),
            tool_name: request.call.name.clone(),
            ok: true,
            summary: "admitted".into(),
            model_content: format!("admitted {item_id}"),
            artifact_ref: None,
            metadata: serde_json::json!({}),
        };
        Ok(ToolOutcome::RuntimeDirective {
            output,
            directive: RuntimeDirective::Context(ContextAction::Admit {
                item_id: item_id
                    .parse()
                    .expect("valid item id from the model script"),
                reason: "the model needs the externalized step again".into(),
            }),
        })
    }
}

/// A model that first asks for the admit (one `context.manage` call), then
/// completes — enough to drive the directive through one real turn.
#[derive(Debug)]
pub(crate) struct AdmitScriptedModel {
    pub(crate) target: agent_contracts::ContextItemId,
    pub(crate) calls: std::sync::atomic::AtomicUsize,
}

impl AdmitScriptedModel {
    pub(crate) fn new(target: agent_contracts::ContextItemId) -> Self {
        Self {
            target,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ModelTransport for AdmitScriptedModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if index == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "admit-1".into(),
                    name: agent_contracts::CONTEXT_MANAGE.into(),
                    arguments: serde_json::json!({
                        "op": "admit",
                        "item_id": self.target.to_string(),
                        "reason": "the model needs this step again",
                    }),
                }],
                usage: Default::default(),
            })
        } else {
            Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }
}

#[derive(Debug)]
pub(crate) struct QuietModel;

#[async_trait::async_trait]
impl ModelTransport for QuietModel {
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
    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        if request.cancel.is_cancelled() {
            return Err(agent_contracts::AgentError::Cancelled);
        }
        sink.on_chunk(ModelChunk::Done).await?;
        Ok(ModelOutput {
            content: "ok".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

pub(crate) fn services() -> RuntimeServices {
    RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )
}

/// Records every lifecycle call so the test can assert the bracket order.
#[derive(Debug)]
pub(crate) struct LifecycleModule {
    pub(crate) log: Arc<Mutex<Vec<String>>>,
    pub(crate) fail_stop: bool,
}

#[async_trait::async_trait]
impl Module for LifecycleModule {
    fn name(&self) -> &'static str {
        "lifecycle"
    }
    fn capabilities(&self) -> Vec<CapabilityId> {
        Vec::new()
    }
    fn register(&self, _registry: &mut ServiceRegistry) -> AgentResult<()> {
        Ok(())
    }
    async fn start(&self) -> AgentResult<()> {
        self.log.lock().unwrap().push("start".into());
        Ok(())
    }
    async fn stop(&self) -> AgentResult<()> {
        self.log.lock().unwrap().push("stop".into());
        if self.fail_stop {
            Err(agent_contracts::AgentError::Internal(
                "module stop failed".into(),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub(crate) struct FailRestoreEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailRestoreEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::RuntimeRestored { .. }) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated restore-commit journal failure".into(),
            ));
        }
        Ok(())
    }
}

/// Holds the first restore's durability barrier so a second caller can try
/// to enter `RuntimeInstance::restore`. The instance-level gate must keep the
/// second prepare out until this barrier is released.
#[derive(Debug)]
pub(crate) struct BlockingFirstRestoreJournal {
    pub(crate) restore_flushes: std::sync::atomic::AtomicUsize,
    pub(crate) first_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(crate) release_first: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl BlockingFirstRestoreJournal {
    pub(crate) fn new(
        first_entered: tokio::sync::oneshot::Sender<()>,
        release_first: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        Self {
            restore_flushes: std::sync::atomic::AtomicUsize::new(0),
            first_entered: Mutex::new(Some(first_entered)),
            release_first: tokio::sync::Mutex::new(Some(release_first)),
        }
    }
}

#[async_trait::async_trait]
impl EventJournal for BlockingFirstRestoreJournal {
    async fn append(&self, _envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        Ok(())
    }

    async fn flush(&self) -> AgentResult<()> {
        // `start()` only appends; the first flush therefore belongs to the
        // first RuntimeRestored durability barrier.
        if self.restore_flushes.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(entered) = self.first_entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release_first.lock().await.take() {
                let _ = release.await;
            }
        }
        Ok(())
    }
}

/// A journal that refuses the typed completion event, so the runtime must
/// surface an audit gap *after* the completion committed instead of
/// pretending the outcome never happened.
#[derive(Debug)]
pub(crate) struct FailCompletionEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailCompletionEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::TaskCompleted { .. }) {
            return Err(agent_contracts::AgentError::Storage(
                "simulated completion event journal failure".into(),
            ));
        }
        Ok(())
    }
}

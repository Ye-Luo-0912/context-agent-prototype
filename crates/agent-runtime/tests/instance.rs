//! `RuntimeInstance` shutdown tests: the instance owns the host, the handle
//! and the actor task, and `shutdown` runs the ordered teardown while
//! aggregating errors instead of swallowing them.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentResult, Capability, CapabilityActivation, CapabilityInvocationContext,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ContextAction, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextSearchQuery, ContextStateTransition, EventJournal, FocusState, MaterializedContext,
    ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput, ModelRequest, ModelTransport,
    RuntimeDirective, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind, TaskId, ToolCall,
    ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolOutput, ToolRisk,
    ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    AuthorityRecoveryServices, CapabilityId, ContextRootClaim, Module, ModuleHost, RootClaimRole,
    RootClaimStrength, RuntimeInstance, RuntimeServices, ServiceRegistry, TaskAnchor,
};
use agent_storage::FileOperationJournal;

/// Build an instance over the real reference engine, so the checkpoint test
/// exercises items, scopes and focus — not a stub that trivially roundtrips.
async fn simple_instance() -> (RuntimeInstance, Arc<context_simple::SimpleContextEngine>) {
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
async fn durable_simple_instance(
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
struct TestContextEngine;

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
struct FailingCompleteEngine;

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

struct CheckpointCapability {
    manifest: CapabilityManifest,
}

impl CheckpointCapability {
    fn new() -> Self {
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
struct EmptyTools;

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
struct AdmitDirectiveDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for AdmitDirectiveDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: agent_contracts::CONTEXT_MANAGE.into(),
            description: "admit a ref back into the working set".into(),
            input_schema: serde_json::json!({ "type": "object" }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
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
struct AdmitScriptedModel {
    target: agent_contracts::ContextItemId,
    calls: std::sync::atomic::AtomicUsize,
}

impl AdmitScriptedModel {
    fn new(target: agent_contracts::ContextItemId) -> Self {
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
struct QuietModel;

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

fn services() -> RuntimeServices {
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
struct LifecycleModule {
    log: Arc<Mutex<Vec<String>>>,
    fail_stop: bool,
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

#[tokio::test]
async fn shutdown_stops_modules_and_joins_the_actor() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: log.clone(),
        fail_stop: false,
    }))
    .unwrap();
    host.start().await.unwrap();

    let services = services();
    let instance = RuntimeInstance::spawn(host, services);
    // Subscribe through the handle: the actor's kernel is derived inside
    // the spawn seam, so the handle is the one live event source.
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance.shutdown().await.unwrap();

    let order = log.lock().unwrap();
    assert_eq!(
        &order[..],
        &["start", "stop"],
        "the module lifecycle must bracket the run"
    );
    drop(order);

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(
        run_completed,
        "shutdown must flush the kernel (RunCompleted) before stopping modules"
    );
}

#[tokio::test]
async fn shutdown_aggregates_module_stop_errors() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: log.clone(),
        fail_stop: true,
    }))
    .unwrap();
    host.start().await.unwrap();

    let instance = RuntimeInstance::spawn(host, services());
    instance.start().await.unwrap();
    let error = instance.shutdown().await.unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("module host stop") && message.contains("module stop failed"),
        "shutdown must aggregate the module failure, got: {message}"
    );
    // The actor task still joined even though the module stop failed.
    let order = log.lock().unwrap();
    assert_eq!(&order[..], &["start", "stop"]);
}

#[tokio::test]
async fn shutdown_with_no_turn_is_a_clean_noop_path() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: Arc::new(Mutex::new(Vec::new())),
        fail_stop: false,
    }))
    .unwrap();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services());
    // Never started the actor; shutdown must still complete within a bounded
    // time (cancel is a no-op, stop is a no-op, host stops, task joins).
    let result = tokio::time::timeout(Duration::from_secs(2), instance.shutdown())
        .await
        .expect("shutdown must not hang");
    assert!(result.is_ok());
}

#[tokio::test]
async fn runtime_checkpoint_roundtrips_tasks_context_and_capabilities() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    let mut events = instance.handle().subscribe();

    // Two tasks, one with a real turn, so the task table and the context
    // engine both carry state worth restoring.
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .user_message("task A: refactor auth".into())
        .await
        .unwrap();
    // Wait for the turn to finish (checkpoint requires an idle runtime).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let completed = events
            .try_recv()
            .is_ok_and(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted));
        if completed || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();

    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(
        checkpoint.tasks.tasks.len(),
        2,
        "the checkpoint must carry the task table, not just the context engine"
    );
    assert!(checkpoint.current_task_id.is_some());
    assert!(
        checkpoint.context != serde_json::Value::Null,
        "the context payload must be present"
    );

    // The file roundtrip: serialize to JSON, parse it back.
    let bytes = serde_json::to_vec(&checkpoint).unwrap();
    let decoded: agent_runtime::RuntimeCheckpoint = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.tasks.tasks.len(), 2);
    assert_eq!(decoded.version, agent_runtime::RUNTIME_CHECKPOINT_VERSION);
    assert!(
        decoded.authority.is_some(),
        "a durable composition must capture its Core authority marker"
    );

    // Restore into a fresh runtime: tasks come back, and the engine carries
    // the restored items and scopes. Shut down the old owner first so the
    // authority journal's exclusive writer lock transfers cleanly.
    instance.shutdown().await.unwrap();
    let (fresh, fresh_context) = durable_simple_instance(&journal_path).await;
    fresh.restore(decoded).await.unwrap();
    let tasks = fresh.handle().list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 2, "restore must bring the task table back");
    let active = tasks
        .iter()
        .find(|task| task.id == checkpoint.current_task_id.unwrap());
    assert_eq!(
        active.map(|task| task.status),
        Some(agent_runtime::TaskStatus::Active),
        "the restored active task must stay active"
    );
    let items = fresh.handle().inspect_context(usize::MAX).await.unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.kind == agent_contracts::ContextKind::UserMessage),
        "the restored engine must carry the user message items"
    );
    // Task id alignment survives the round-trip: the restored engine's
    // focus must point at the same task the runtime restored as current,
    // so runtime and context cannot drift into a split-brain after
    // recovery.
    let restored_focus = fresh_context
        .materialize(ContextQuery {
            current_input: "resume".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap()
        .focus;
    assert_eq!(
        restored_focus.map(|focus| focus.task_id),
        checkpoint.current_task_id,
        "restore must align the context focus with the runtime's current task"
    );
    fresh.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_checkpoint_cannot_restore_across_core_instances() {
    let (source, _context) = simple_instance().await;
    let checkpoint = source.checkpoint().await.unwrap();
    assert!(checkpoint.authority.is_none());

    let (fresh, _context) = simple_instance().await;
    let before = fresh.handle().list_tasks().await.unwrap();
    let error = fresh.restore(checkpoint).await.unwrap_err();
    assert!(
        error.to_string().contains("ephemeral checkpoint")
            && error.to_string().contains("cannot restore"),
        "cross-Core ephemeral restore must fail before mutation: {error}"
    );
    assert_eq!(fresh.handle().list_tasks().await.unwrap(), before);

    fresh.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn tampered_authority_marker_is_rejected_before_runtime_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    instance
        .handle()
        .set_focus("live task must survive rejected restore".into())
        .await
        .unwrap();
    let before = instance.checkpoint().await.unwrap();
    let mut tampered = before.clone();
    tampered.tasks.tasks[0].goal = "must never become visible".into();
    tampered
        .authority
        .as_mut()
        .expect("durable checkpoint carries authority")
        .state_digest = agent_contracts::AuthorityStateDigest::sha256_bytes(b"tampered");

    let error = instance.restore(tampered).await.unwrap_err();
    assert!(
        error.to_string().contains("authority checkpoint marker"),
        "authority mismatch must be explicit: {error}"
    );
    let after = instance.checkpoint().await.unwrap();
    assert_eq!(after.tasks.tasks[0].goal, before.tasks.tasks[0].goal);
    assert_eq!(after.focus_revision, before.focus_revision);
    assert_eq!(after.last_surface_revision, before.last_surface_revision);
    assert_eq!(
        after.authority, before.authority,
        "validation must not bump Core"
    );

    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_checkpoint_marker_remains_valid_after_later_epoch_transitions() {
    let temp = tempfile::tempdir().unwrap();
    let journal_path = temp.path().join("authority").join("operations.jsonl");
    let (instance, _context) = durable_simple_instance(&journal_path).await;
    instance
        .handle()
        .set_focus("checkpoint ancestor".into())
        .await
        .unwrap();
    let checkpoint = instance.checkpoint().await.unwrap();
    let checkpoint_marker = checkpoint.authority.clone().unwrap();

    instance.handle().suspend_task().await.unwrap();
    let advanced = instance.checkpoint().await.unwrap().authority.unwrap();
    assert!(advanced.last_seq > checkpoint_marker.last_seq);
    assert!(advanced.authority_epoch > checkpoint_marker.authority_epoch);

    instance.restore(checkpoint).await.unwrap();
    let restored = instance.checkpoint().await.unwrap();
    assert_eq!(restored.current_task_id, restored.tasks.active);
    assert_eq!(restored.tasks.tasks[0].goal, "checkpoint ancestor");
    assert!(
        restored.authority.unwrap().last_seq > advanced.last_seq,
        "restore advances live authority; it never rewinds to the checkpoint cursor"
    );

    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_restore_keeps_the_existing_task_and_context_authority() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();

    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.tasks[0].goal = "replacement task".into();
    // The task half is internally valid, but the opaque context payload is
    // not. Previously the actor installed the replacement task table before
    // discovering this restore error.
    invalid.context = serde_json::Value::Null;

    let result = instance.restore(invalid).await;
    assert!(
        result.is_err(),
        "the invalid context payload must be rejected"
    );
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].goal, "original task");

    let focus = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap()
        .focus
        .expect("the original context focus must survive failed restore");
    assert_eq!(focus.task_id, tasks[0].id);
    assert_eq!(focus.goal, "original task");
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_inconsistent_redundant_task_authority() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.active = None;

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("task authority is inconsistent"),
        "the redundant authority mismatch must be diagnosed before mutation: {error}"
    );
    assert_eq!(instance.handle().list_tasks().await.unwrap(), before);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_context_focus_that_disagrees_with_task_authority() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();

    // Keep all actor-owned redundant fields internally consistent while
    // making them disagree with the opaque context checkpoint's focus.
    let replacement = agent_contracts::TaskId::new();
    invalid.tasks.tasks[0].id = replacement;
    invalid.tasks.active = Some(replacement);
    invalid.current_task_id = Some(replacement);

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("context focus"),
        "context/task disagreement must be rejected explicitly: {error}"
    );
    assert_eq!(instance.handle().list_tasks().await.unwrap(), before);
    let focus = context
        .materialize(ContextQuery {
            current_input: String::new(),
            budget_tokens: 0,
            hints: Default::default(),
        })
        .await
        .unwrap()
        .focus
        .unwrap();
    assert_eq!(focus.task_id, before[0].id);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_actor_restore_does_not_change_capability_flags() {
    let host = ModuleHost::new();
    host.register_capability(Arc::new(CheckpointCapability::new()))
        .unwrap();
    let registry = host.capability_registry();
    registry.enable("checkpoint-capability").unwrap();
    registry.load_tool("checkpoint.tool").unwrap();

    let instance = RuntimeInstance::spawn(host, services());
    instance.start().await.unwrap();
    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.version += 1;
    invalid.capabilities[0].activation = CapabilityActivation::Disabled;
    invalid.capabilities[0].loaded = false;

    assert!(instance.restore(invalid).await.is_err());
    assert_eq!(
        registry.activation("checkpoint-capability"),
        Some(CapabilityActivation::Enabled),
        "a rejected actor restore must not partially apply capability activation"
    );
    assert_eq!(
        registry.tool_state("checkpoint.tool"),
        Some(ToolLifecycle::Loaded),
        "a rejected actor restore must not unload the existing surface"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_event_reports_only_capabilities_registered_in_the_live_host() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    let mut checkpoint = instance.checkpoint().await.unwrap();
    checkpoint
        .capabilities
        .push(agent_runtime::CapabilitySnapshot {
            id: "missing-from-live-host".into(),
            activation: CapabilityActivation::Enabled,
            loaded: true,
            loaded_tools: vec!["missing.tool".into()],
        });

    instance.restore(checkpoint).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let applied = loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "restore event was not published");
        let envelope = tokio::time::timeout(remaining, events.recv())
            .await
            .expect("restore event timeout")
            .expect("restore event channel closed");
        if let RuntimeEvent::RuntimeRestored {
            capabilities_applied,
            ..
        } = envelope.event
        {
            break capabilities_applied;
        }
    };
    assert!(
        !applied,
        "an unknown-only checkpoint capability set must not be reported as applied"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_full_restores_are_serialized_across_all_state_planes() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let journal = Arc::new(BlockingFirstRestoreJournal::new(entered_tx, release_rx));
    let instance = Arc::new(RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContextEngine),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(journal.clone()),
        ),
    ));
    let checkpoint_a = agent_runtime::RuntimeCheckpoint {
        version: agent_runtime::RUNTIME_CHECKPOINT_VERSION,
        run_metadata: agent_runtime::RunMetadata {
            run_id: instance.handle().run_id(),
            created_at_ms: 0,
        },
        tasks: agent_runtime::TaskManagerSnapshot {
            tasks: Vec::new(),
            active: None,
            completed: Vec::new(),
        },
        current_task_id: None,
        focus_revision: 0,
        last_surface_revision: 0,
        context: serde_json::Value::Null,
        capabilities: Vec::new(),
        authority: None,
    };
    let mut checkpoint_b = checkpoint_a.clone();
    checkpoint_b.focus_revision = 41;

    let first_instance = instance.clone();
    let first = tokio::spawn(async move { first_instance.restore(checkpoint_a).await });
    tokio::time::timeout(Duration::from_secs(2), entered_rx)
        .await
        .expect("first restore did not reach its durable finalize barrier")
        .expect("first restore dropped the barrier signal");

    let second_instance = instance.clone();
    let second = tokio::spawn(async move { second_instance.restore(checkpoint_b).await });

    // If the full transaction were not gated, B could prepare while A is
    // blocked at finalize, replacing A's pending token and eventually
    // allowing a late capability-plane write after B unfenced the actor.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second.is_finished(),
        "the second full restore crossed the first restore transaction"
    );

    release_tx.send(()).expect("release first restore barrier");
    tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first restore did not finish")
        .expect("first restore task panicked")
        .expect("first restore failed");
    tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("second restore did not finish")
        .expect("second restore task panicked")
        .expect("second restore failed");

    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.focus_revision > 41,
        "the second restore must commit after the first, not interleave with it"
    );
    let instance = Arc::try_unwrap(instance).unwrap_or_else(|_| panic!("test leaked instance"));
    instance.shutdown().await.unwrap();
}

#[derive(Debug)]
struct FailRestoreEventJournal;

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
struct BlockingFirstRestoreJournal {
    restore_flushes: std::sync::atomic::AtomicUsize,
    first_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_first: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl BlockingFirstRestoreJournal {
    fn new(
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
struct FailCompletionEventJournal;

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

#[tokio::test]
async fn restore_emits_the_bounded_restore_commit_event() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();
    let task_a = instance.handle().list_tasks().await.unwrap()[0].id;
    let checkpoint = instance.checkpoint().await.unwrap();

    // Push task A's tool-requirement revision past the checkpoint's value,
    // so restoring the older checkpoint must rebase it (CAS-ABA fence).
    instance
        .handle()
        .replace_task_tool_requirements(task_a, 0, Vec::new())
        .await
        .unwrap();

    instance.restore(checkpoint.clone()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut restored = None;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::RuntimeRestored { .. } = envelope.event {
                restored = Some(envelope.event);
                break;
            }
        }
        if restored.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let RuntimeEvent::RuntimeRestored {
        checkpoint_version,
        restored_run_id,
        current_run_id,
        focus_revision,
        surface_revision,
        rebased_tasks,
        rebased_task_sample,
        capabilities_applied,
    } = restored.expect("restore must publish its bounded restore-commit event")
    else {
        panic!("restore event is not the expected variant");
    };

    assert_eq!(
        checkpoint_version,
        agent_runtime::RUNTIME_CHECKPOINT_VERSION,
        "the event names the restored checkpoint version"
    );
    assert_eq!(
        restored_run_id, current_run_id,
        "an in-process round-trip restores the same run"
    );
    assert_eq!(
        restored_run_id, checkpoint.run_metadata.run_id,
        "the event names the run that produced the checkpoint"
    );
    assert!(
        focus_revision.effective > focus_revision.old,
        "restore bumps the focus revision into a fresh epoch: {focus_revision:?}"
    );
    assert!(
        surface_revision.effective >= surface_revision.restored
            && surface_revision.effective >= surface_revision.old,
        "the surface revision never moves backwards: {surface_revision:?}"
    );
    // Both tasks carry a tool-requirement revision at or below the live
    // high-water mark (task A was advanced, task B ties), so both are
    // rebased forward.
    assert_eq!(
        rebased_tasks, 2,
        "the event counts every rebased task requirement revision"
    );
    assert_eq!(
        rebased_task_sample.len(),
        2,
        "the capped sample carries the rebased task ids"
    );
    assert!(
        !capabilities_applied,
        "an empty capability surface records nothing applied"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_audit_failure_demands_recovery_and_fences_mutation() {
    let (source, _context) = simple_instance().await;
    source
        .handle()
        .set_focus("original task".into())
        .await
        .unwrap();
    let mut checkpoint = source.checkpoint().await.unwrap();

    // The actor with a journal that refuses the restore-commit record.
    let failing = RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            // The real reference engine: restore must pass the
            // context/task focus agreement check and reach the journal
            // barrier before the audit failure can surface.
            Arc::new(context_simple::SimpleContextEngine::new(
                context_simple::SimpleContextConfig::default(),
            )),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(Arc::new(FailRestoreEventJournal)),
        ),
    );
    failing.start().await.unwrap();
    // This test isolates the final event-journal barrier. With no durable
    // operation journal, an ephemeral checkpoint is valid only inside the
    // same live run, so align its provenance with this deliberately bare
    // test Core before exercising the later audit failure.
    checkpoint.run_metadata.run_id = failing.handle().run_id();
    let mut events = failing.handle().subscribe();

    let error = failing.restore(checkpoint).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("simulated restore-commit journal failure"),
        "the audit failure must surface from restore: {error}"
    );

    // The standard recovery signal is emitted when possible.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_recovery = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::RecoveryRequired) {
                saw_recovery = true;
            }
        }
        if saw_recovery {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_recovery, "the runtime must emit the recovery signal");

    // Normal mutation is rejected until a known-good restore lands.
    let fenced = failing
        .handle()
        .set_focus("another task".into())
        .await
        .unwrap_err();
    assert!(
        fenced.to_string().contains("recovery is required"),
        "mutation must be fenced after a restore whose audit event failed: {fenced}"
    );
    failing.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_anchor_update_publishes_a_bounded_event() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let anchor = TaskAnchor {
        original_goal: "refactor auth".into(),
        current_interpretation: "split the auth module".into(),
        acceptance_criteria: vec!["tests pass".into()],
        open_loops: vec!["verify edge cases".into()],
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_id, 0, anchor)
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // The bounded audit event names the task, the resulting revision, the
    // fields that moved and the authority split — never the full anchor
    // content.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged {
                task_id: event_task,
                revision: event_rev,
                changed_fields,
                patch_kind,
            } = envelope.event
            {
                saw = Some((event_task, event_rev, changed_fields, patch_kind));
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let (event_task, event_rev, changed_fields, patch_kind) =
        saw.expect("anchor update must publish its event");
    assert_eq!(event_task, task_id);
    assert_eq!(event_rev, 1);
    assert_eq!(
        patch_kind,
        agent_contracts::AnchorPatchKind::Autonomous,
        "a whole-anchor replacement that moves only evolution fields is labeled autonomous"
    );
    for field in [
        "current_interpretation",
        "acceptance_criteria",
        "open_loops",
    ] {
        assert!(
            changed_fields.iter().any(|name| name == field),
            "the event must name the moved field {field}: {changed_fields:?}"
        );
    }
    assert!(
        !changed_fields.iter().any(|name| name == "original_goal"),
        "an unchanged field must not be named: {changed_fields:?}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn autonomous_anchor_patch_applies_without_approval() {
    // read-only approval: autonomous patches never consult the gate, so
    // they land even under the strictest policy.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let revision = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                plan_progress: Some(vec!["read the module".into()]),
                open_loops: Some(vec!["verify edge cases".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged { patch_kind, .. } = envelope.event {
                saw = Some(patch_kind);
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(
        saw,
        Some(agent_contracts::AnchorPatchKind::Autonomous),
        "the audit event must label the autonomous patch"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_anchor_patch_clears_approval_and_is_labeled_boundary() {
    // permissive approval: the boundary patch (constraints) passes the gate.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let revision = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                constraints: Some(vec!["no dependency changes".into()]),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(revision, 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskAnchorChanged { patch_kind, .. } = envelope.event {
                saw = Some(patch_kind);
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert_eq!(
        saw,
        Some(agent_contracts::AnchorPatchKind::Boundary),
        "the audit event must label the boundary patch"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn boundary_anchor_patch_denied_leaves_the_anchor_untouched() {
    // read-only approval: a boundary patch (goal) is denied and must not
    // reach the task table — no revision bump, no change event.
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let denied = instance
        .handle()
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                original_goal: Some("rewrite from scratch".into()),
                ..agent_runtime::AnchorPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        denied.to_string().contains("denied by approval policy"),
        "a denied boundary patch must error with the policy message: {denied}"
    );

    // No change event was published and the anchor is untouched.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut saw_change = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TaskAnchorChanged { .. }) {
                saw_change = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !saw_change,
        "a denied patch must not publish a change event"
    );
    assert_eq!(
        instance.handle().list_tasks().await.unwrap()[0].anchor_revision,
        0,
        "a denied patch must not bump the anchor revision"
    );
    instance.shutdown().await.unwrap();
}

/// Full-suite acceptance for `context.admit`: a model tool call travels the
/// whole route — scripted model emits `context.manage`, the dispatcher
/// returns the typed runtime directive, the actor executes it at
/// operation-commit time, and the engine re-enters the externalized item
/// under its original id. The result is the admission event itself, not a
/// duplicated observation.
#[tokio::test]
async fn context_manage_admit_routes_end_to_end() {
    // 种入一个被外部化的条目（buffer 溢出触发 full GC 写入 store）。
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig {
            gc_buffer_capacity: 1,
            gc_reactivate_per_pass: 8,
            context_store_dir: Some(dir.path().to_path_buf()),
            ..context_simple::SimpleContextConfig::default()
        },
    ));
    context
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "service layer"),
        })
        .await
        .unwrap();
    context
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for (index, ch) in ["x", "y"].iter().enumerate() {
        context
            .ingest(ContextIngress::ToolObservation {
                output: ToolOutput {
                    call_id: format!("step-{index}"),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: format!("step {index}: fix AuthService.rs {}", ch.repeat(160)),
                    artifact_ref: None,
                    metadata: serde_json::json!({}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
    }
    context
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc_report = context.gc().await.unwrap();
    assert!(gc_report.externalized >= 1, "the seed must externalize");

    let refs = context
        .search_external(ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            label: None,
            limit: 16,
        })
        .await
        .unwrap();
    let target = refs[0].item_id;
    let resident_before = context.diagnostics().await.unwrap().resident_items;

    // 构造实例：真实 context 引擎 + 返回 admit 指令的 dispatcher +
    // 先发工具调用后完成的 scripted model。
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(AdmitScriptedModel::new(target)),
        Arc::new(AdmitDirectiveDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    let instance = RuntimeInstance::spawn(ModuleHost::new(), services);
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("service layer".into())
        .await
        .unwrap();
    instance
        .handle()
        .user_message("admit the step I need".into())
        .await
        .unwrap();

    // 等待 turn 完成（admit 指令在操作提交时执行）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let completed = events
            .try_recv()
            .is_ok_and(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted));
        if completed || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        context.diagnostics().await.unwrap().resident_items > resident_before,
        "the admitted item must be resident after the turn"
    );
    let inspected = context.inspect(100).await.unwrap();
    assert!(
        inspected.iter().any(|item| item.id == target),
        "the admitted item is inspectable under its original id"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_anchor_survives_checkpoint_restore() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    let anchor = TaskAnchor {
        original_goal: "refactor auth".into(),
        current_interpretation: "split the auth module".into(),
        constraints: vec!["no dependency changes".into()],
        acceptance_criteria: vec!["tests pass".into()],
        plan_progress: vec!["read the module".into()],
        open_loops: vec!["verify edge cases".into()],
        working_refs: vec![ContextRootClaim {
            item_ref: "item:auth".into(),
            role: RootClaimRole::ActiveDecision,
            strength: RootClaimStrength::ResidentRequired,
            source_field_id: "plan_progress".into(),
        }],
        evidence_refs: Vec::new(),
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_id, 0, anchor)
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // The checkpoint carries the full anchor (authority, not a scored item).
    let checkpoint = instance.checkpoint().await.unwrap();
    let snapshot = &checkpoint.tasks.tasks[0];
    assert_eq!(snapshot.anchor.revision, 1);
    assert_eq!(
        snapshot.anchor.current_interpretation,
        "split the auth module"
    );
    assert_eq!(
        snapshot.anchor.working_refs[0].role,
        RootClaimRole::ActiveDecision
    );

    // Restoring the checkpoint brings the anchor back, revision intact
    // (anchor revisions are task authority, never rebased like surface
    // revisions).
    instance.restore(checkpoint).await.unwrap();
    let info = &instance.handle().list_tasks().await.unwrap()[0];
    assert_eq!(info.id, task_id);
    assert_eq!(info.anchor_revision, 1);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_commits_a_typed_record_and_publishes_task_identity() {
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;

    // Advance the anchor so the completion record names a non-trivial
    // revision: the outcome is measured against exactly that authority.
    instance
        .handle()
        .update_task_anchor(
            task_id,
            0,
            TaskAnchor {
                original_goal: "refactor auth".into(),
                acceptance_criteria: vec!["tests pass".into()],
                ..TaskAnchor::default()
            },
        )
        .await
        .unwrap();

    instance
        .handle()
        .complete_current_task("auth refactor shipped".into())
        .await
        .unwrap();

    // The typed event carries the task/result identity, not free text only.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TaskCompleted {
                task_id: event_task,
                anchor_revision,
                summary,
            } = envelope.event
            {
                saw = Some((event_task, anchor_revision, summary));
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let (event_task, anchor_revision, summary) =
        saw.expect("completion must publish its typed event");
    assert_eq!(event_task, task_id);
    assert_eq!(anchor_revision, 1);
    assert_eq!(summary, "auth refactor shipped");

    // The checkpoint carries the immutable record; restore brings it back.
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1);
    assert_eq!(checkpoint.tasks.completed[0].task_id, task_id);
    assert_eq!(checkpoint.tasks.completed[0].anchor_revision, 1);
    assert_eq!(
        checkpoint.tasks.completed[0].summary,
        "auth refactor shipped"
    );
    assert_eq!(
        checkpoint.tasks.tasks[0].status,
        agent_runtime::TaskStatus::Completed
    );

    instance.restore(checkpoint).await.unwrap();
    assert_eq!(
        instance.handle().list_tasks().await.unwrap()[0].status,
        agent_runtime::TaskStatus::Completed,
        "the completed task stays completed after restore"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn restore_rejects_completed_task_without_a_completion_record() {
    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap();

    let mut invalid = instance.checkpoint().await.unwrap();
    invalid.tasks.completed.clear();

    let error = instance.restore(invalid).await.unwrap_err();
    assert!(
        error.to_string().contains("no committed completion record"),
        "a completed task must own exactly one outcome: {error}"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn task_completion_schedules_storage_gc_and_publishes_the_report() {
    // Task completion is the explicit runtime boundary for Storage GC: the
    // completed task's records stop being storage roots at this point, so
    // one conservative Storage GC pass runs right after the completion GC —
    // never on the per-model hot path — and its report is published as a
    // `StorageGc` event, the only permanent-deletion surface.
    let (instance, _context) = simple_instance().await;
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = None;
    while tokio::time::Instant::now() < deadline && saw.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::StorageGc { report } = envelope.event {
                saw = Some(report);
                break;
            }
        }
        if saw.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let report = saw.expect("task completion must schedule one storage GC pass");
    // The empty engine has nothing to delete, but the pass still ran and
    // reported; io_errors stay 0 — an IO failure is never mistaken for
    // "the file is already gone".
    assert_eq!(report.io_errors, 0);
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_failure_never_leaves_a_half_closed_task() {
    // The context side refuses the completion ingest: the transaction must
    // fail before the task authority plane commits, so the task stays
    // Active, the active slot stays, and no outcome record exists.
    let instance = RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(FailingCompleteEngine),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ),
    );
    instance.start().await.unwrap();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let before = instance.handle().list_tasks().await.unwrap();

    let error = instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("simulated completion ingest failure"),
        "the completion failure must surface: {error}"
    );

    // No half-closed task: still Active, active slot intact, no record.
    let after = instance.handle().list_tasks().await.unwrap();
    assert_eq!(after, before, "a failed completion changes nothing");
    assert_eq!(after[0].status, agent_runtime::TaskStatus::Active);
    let checkpoint = instance.checkpoint().await.unwrap();
    assert!(
        checkpoint.tasks.completed.is_empty(),
        "no outcome was committed"
    );
    assert_eq!(checkpoint.current_task_id, before[0].id.into());
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_audit_gap_marks_recovery_but_keeps_the_commit() {
    // The completion itself commits (context + task authority together), but
    // the mandatory typed event cannot be journaled: the runtime must keep
    // the aligned committed state, mark recovery-required and emit the
    // standard recovery signal — never report an un-audited success.
    let instance = RuntimeInstance::spawn(
        ModuleHost::new(),
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(context_simple::SimpleContextEngine::new(
                context_simple::SimpleContextConfig::default(),
            )),
            Arc::new(QuietModel),
            Arc::new(EmptyTools),
            Arc::new(PolicyApprovalGate::read_only()),
            Some(Arc::new(FailCompletionEventJournal)),
        ),
    );
    instance.start().await.unwrap();
    let mut events = instance.handle().subscribe();
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();

    let error = instance
        .handle()
        .complete_current_task("shipped".into())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("audit event failed"),
        "the audit gap must surface explicitly: {error}"
    );

    // The standard recovery signal is emitted.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_recovery = false;
    while tokio::time::Instant::now() < deadline && !saw_recovery {
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::RecoveryRequired) {
                saw_recovery = true;
            }
        }
        if !saw_recovery {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(saw_recovery, "the runtime must emit the recovery signal");

    // The aligned state stayed committed: the task is Completed and the
    // runtime fences normal mutation until a known-good restore. (Checkpoint
    // itself is refused while recovery is required — that refusal is part of
    // the fence, and restore is the one mutation that may clear it.)
    let tasks = instance.handle().list_tasks().await.unwrap();
    assert_eq!(tasks[0].status, agent_runtime::TaskStatus::Completed);
    let fenced = instance
        .handle()
        .set_focus("another task".into())
        .await
        .unwrap_err();
    assert!(
        fenced.to_string().contains("recovery is required"),
        "mutation must be fenced after an audit gap: {fenced}"
    );
    let fenced_checkpoint = instance.checkpoint().await.unwrap_err();
    assert!(
        fenced_checkpoint
            .to_string()
            .contains("recovery is required"),
        "checkpoint must also be fenced while recovery is required"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn thousand_completed_tasks_stay_bounded_and_searchable() {
    let (instance, context) = simple_instance().await;
    for index in 0..1000 {
        instance
            .handle()
            .set_focus(format!("task {index}: fix component {index}"))
            .await
            .unwrap();
        instance
            .handle()
            .complete_current_task(format!("component {index} fixed"))
            .await
            .unwrap();
    }

    // Every completed task owns exactly one committed outcome: the runtime's
    // task catalog holds all of them, and the checkpoint persists each one.
    assert_eq!(instance.handle().list_tasks().await.unwrap().len(), 1000);
    let checkpoint = instance.checkpoint().await.unwrap();
    assert_eq!(checkpoint.tasks.completed.len(), 1000);
    assert!(
        checkpoint
            .tasks
            .tasks
            .iter()
            .all(|task| task.status == agent_runtime::TaskStatus::Completed)
    );

    // The context working set stays bounded: completing 1,000 tasks must not
    // grow the resident heap linearly with the task count. A completed
    // task's records are storage roots, not residency roots.
    let diagnostics = context.diagnostics().await.unwrap();
    assert!(
        diagnostics.resident_items < 200,
        "resident heap must stay bounded after 1,000 completed tasks, got {}",
        diagnostics.resident_items
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_record_carries_a_verifiable_final_output_digest() {
    use sha2::{Digest, Sha256};

    let (instance, _context) = simple_instance().await;
    instance
        .handle()
        .set_focus("refactor auth".into())
        .await
        .unwrap();
    let task_id = instance.handle().list_tasks().await.unwrap()[0].id;
    let summary = "auth refactor shipped";
    instance
        .handle()
        .complete_current_task(summary.into())
        .await
        .unwrap();

    // The completion record names the exact final-output body and its
    // digest, so the outcome is byte-for-byte verifiable.
    let checkpoint = instance.checkpoint().await.unwrap();
    let record = &checkpoint.tasks.completed[0];
    let mut hasher = Sha256::new();
    hasher.update(summary.as_bytes());
    let expected = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(
        record.final_output_digest.as_deref(),
        Some(expected.as_str()),
        "the record carries the digest of the exact final output"
    );
    assert_eq!(
        record.final_output_ref.as_deref(),
        Some(format!("task:{task_id}:completion").as_str()),
        "the record carries a deterministic ref to its own final output"
    );

    // Restart (restore) keeps the outcome and its digest intact.
    instance.restore(checkpoint).await.unwrap();
    let restored = instance.checkpoint().await.unwrap();
    let restored_record = &restored.tasks.completed[0];
    assert_eq!(restored_record.summary, summary);
    assert_eq!(
        restored_record.final_output_digest.as_deref(),
        Some(expected.as_str()),
        "the digest survives a restore unchanged"
    );
    instance.shutdown().await.unwrap();
}

#[tokio::test]
async fn suspend_and_resume_preserves_anchor_without_replaying_transcript() {
    let (instance, context) = simple_instance().await;
    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    let task_a = instance.handle().list_tasks().await.unwrap()[0].id;
    let anchor = TaskAnchor {
        original_goal: "task A: refactor auth".into(),
        acceptance_criteria: vec!["tests pass".into()],
        open_loops: vec!["verify edge cases".into()],
        ..TaskAnchor::default()
    };
    let revision = instance
        .handle()
        .update_task_anchor(task_a, 0, anchor.clone())
        .await
        .unwrap();
    assert_eq!(revision, 1);

    // Suspend A, work on an unrelated task, run an unrelated GC pass.
    instance
        .handle()
        .set_focus("task B: write docs".into())
        .await
        .unwrap();
    context.gc().await.unwrap();

    // Resume A: the anchor is task authority held by the runtime, not by
    // the transcript — criteria/open loops survive suspension and an
    // unrelated GC without any replay.
    instance.handle().activate_task(task_a).await.unwrap();
    let resumed = instance
        .handle()
        .list_tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == task_a)
        .expect("task A exists");
    assert_eq!(resumed.anchor_revision, 1);
    // An equivalent replacement is idempotent at the restored revision:
    // nothing was lost or rewritten while suspended.
    let equivalent = instance
        .handle()
        .update_task_anchor(task_a, 1, anchor)
        .await
        .unwrap();
    assert_eq!(
        equivalent, 1,
        "an equivalent anchor must not bump revision after resume"
    );
    instance.shutdown().await.unwrap();
}

/// The runtime assigns a task id on focus; the context engine must be
/// focused on the *same* task — runtime and context share one task
/// identity, never a parallel one.
#[tokio::test]
async fn runtime_task_id_matches_the_context_task_id() {
    let (instance, context) = simple_instance().await;
    let mut events = instance.handle().subscribe();

    instance
        .handle()
        .set_focus("task A: refactor auth".into())
        .await
        .unwrap();
    let mut runtime_task_id = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && runtime_task_id.is_none() {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::FocusChanged { task_id, .. } = envelope.event {
                runtime_task_id = Some(task_id);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let runtime_task_id = runtime_task_id.expect("FocusChanged must carry the task id");

    // The engine's materialized focus must carry the same task id the
    // runtime assigned — the single source of task identity, not a copy.
    let snapshot = context
        .materialize(ContextQuery {
            current_input: "continue".into(),
            budget_tokens: 4096,
            hints: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        snapshot.focus.map(|focus| focus.task_id),
        Some(runtime_task_id),
        "the context engine must be focused on the runtime's task id, not a parallel one"
    );
    instance.shutdown().await.unwrap();
}

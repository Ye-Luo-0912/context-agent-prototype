//! `RuntimeInstance` shutdown tests: the instance owns the host, the handle
//! and the actor task, and `shutdown` runs the ordered teardown while
//! aggregating errors instead of swallowing them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentResult, Capability, CapabilityActivation, CapabilityInvocationContext,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ModelCapabilities, ModelChunk, ModelEventSink, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, ScopeId, ScopeKind, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_kernel::{AgentKernel, AgentKernelConfig, PolicyApprovalGate};
use agent_runtime::{CapabilityId, Module, ModuleHost, RuntimeInstance, ServiceRegistry};

/// Build an instance over the real reference engine, so the checkpoint test
/// exercises items, scopes and focus — not a stub that trivially roundtrips.
async fn simple_instance() -> (RuntimeInstance, Arc<context_simple::SimpleContextEngine>) {
    let context = Arc::new(context_simple::SimpleContextEngine::new(
        context_simple::SimpleContextConfig::default(),
    ));
    let kernel = Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        context.clone(),
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let instance = RuntimeInstance::spawn(ModuleHost::new(), kernel);
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

fn kernel() -> Arc<AgentKernel> {
    Arc::new(AgentKernel::new(
        AgentKernelConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(QuietModel),
        Arc::new(EmptyTools),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ))
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

    let kernel = kernel();
    let mut events = kernel.subscribe();
    let instance = RuntimeInstance::spawn(host, kernel);
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

    let instance = RuntimeInstance::spawn(host, kernel());
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
    let instance = RuntimeInstance::spawn(host, kernel());
    // Never started the actor; shutdown must still complete within a bounded
    // time (cancel is a no-op, stop is a no-op, host stops, task joins).
    let result = tokio::time::timeout(Duration::from_secs(2), instance.shutdown())
        .await
        .expect("shutdown must not hang");
    assert!(result.is_ok());
}

#[tokio::test]
async fn runtime_checkpoint_roundtrips_tasks_context_and_capabilities() {
    let (instance, _context) = simple_instance().await;
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

    // Restore into a fresh runtime: tasks come back, and the engine carries
    // the restored items and scopes.
    let (fresh, fresh_context) = simple_instance().await;
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

    let instance = RuntimeInstance::spawn(host, kernel());
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

//! RUNTIME-CONTEXT-COMMIT-01: turn-start context application and checkpoint
//! maintenance commit through the runtime-owned schedule, never silently
//! ahead of task/audit state.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, Capability, CapabilityInvocationContext, CapabilityKind,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    EventJournal, MaterializedContext, RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind,
    ToolCall,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    CapabilityAwareDispatcher, CapabilityRegistry, ModuleHost, RuntimeInstance, RuntimeServices,
    ToolModule, spawn_runtime,
};

use crate::harness::*;

/// Context engine with a serializable body so a test can prove a failed
/// transaction actually rolled the plane back, plus failure injections and
/// a Checkpoint-maintenance counter.
#[derive(Debug, Default)]
struct BodyTrackingEngine {
    state: Mutex<Vec<String>>,
    fail_user_input_maintain: AtomicBool,
    checkpoint_maintains: AtomicUsize,
    slow_checkpoint_ms: AtomicU64,
}

#[async_trait::async_trait]
impl ContextEngine for BodyTrackingEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if let ContextIngress::UserMessage { content } = ingress {
            self.state.lock().unwrap().push(content);
        }
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        if trigger == ContextMaintenanceTrigger::Checkpoint {
            self.checkpoint_maintains.fetch_add(1, Ordering::SeqCst);
        }
        if trigger == ContextMaintenanceTrigger::UserInput
            && self.fail_user_input_maintain.load(Ordering::SeqCst)
        {
            return Err(AgentError::Context(
                "simulated user-input maintenance failure".into(),
            ));
        }
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
        let slow = self.slow_checkpoint_ms.load(Ordering::SeqCst);
        if slow > 0 {
            tokio::time::sleep(Duration::from_millis(slow)).await;
        }
        serde_json::to_value(self.state.lock().unwrap().clone())
            .map_err(|e| AgentError::Internal(format!("test checkpoint: {e}")))
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        *self.state.lock().unwrap() = serde_json::from_value(data)
            .map_err(|e| AgentError::Internal(format!("test restore: {e}")))?;
        Ok(())
    }
}

/// A received user message whose accepted-event append fails: the message
/// was already applied, so the audit gap must fence the runtime.
#[derive(Debug)]
struct FailUserMessageAcceptedJournal;

#[async_trait::async_trait]
impl EventJournal for FailUserMessageAcceptedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::UserMessageAccepted { .. }) {
            return Err(AgentError::Storage(
                "simulated accepted-event journal failure".into(),
            ));
        }
        Ok(())
    }
}

/// A received user message whose UserInput-maintained event append fails:
/// the accepted event already landed, so the gap must still fence.
#[derive(Debug)]
struct FailUserInputMaintainedJournal;

#[async_trait::async_trait]
impl EventJournal for FailUserInputMaintainedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(
            envelope.event,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::UserInput,
                ..
            }
        ) {
            return Err(AgentError::Storage(
                "simulated maintained-event journal failure".into(),
            ));
        }
        Ok(())
    }
}

/// Minimal capability so the instance's registry has a mutable surface for
/// the generation-stability race.
#[derive(Debug)]
struct TestCapability {
    manifest: CapabilityManifest,
}

impl TestCapability {
    fn new(id: &str) -> Self {
        Self {
            manifest: CapabilityManifest {
                id: id.into(),
                version: "1.0.0".into(),
                name: "test".into(),
                summary: "test capability".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![CapabilityKind::Tool],
                permissions: Vec::new(),
                requires: Vec::new(),
                tools: Vec::new(),
                lifecycle: CapabilityLifecycle::Eager,
                transport: CapabilityTransport::Builtin,
                sandbox_profile: Default::default(),
            },
        }
    }
}

#[async_trait::async_trait]
impl Capability for TestCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }
    async fn invoke(
        &self,
        _call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        Err(AgentError::Tool("unused test capability".into()))
    }
}

fn actor_kernel(context: Arc<dyn ContextEngine>) -> Arc<RuntimeServices> {
    Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ))
}

/// A UserInput maintenance failure after the message was ingested must
/// roll the context plane back — the failure is clean (no recovery fence)
/// and the runtime can accept the next message.
#[tokio::test]
async fn user_input_maintain_failure_rolls_back_the_context_plane() {
    let engine = Arc::new(BodyTrackingEngine::default());
    engine
        .fail_user_input_maintain
        .store(true, Ordering::SeqCst);
    let (handle, _task) = spawn_runtime(actor_kernel(engine.clone()));
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    let error = handle.user_message("hello".into()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("simulated user-input maintenance failure"),
        "the maintenance failure must surface: {error}"
    );
    assert!(
        !matches!(error, AgentError::RecoveryRequired(_)),
        "a successful rollback must not fence the runtime: {error}"
    );
    assert!(
        engine.state.lock().unwrap().is_empty(),
        "the context plane must roll the ingested body back, got: {:?}",
        engine.state.lock().unwrap()
    );

    // The runtime stays usable: the next message commits normally.
    engine
        .fail_user_input_maintain
        .store(false, Ordering::SeqCst);
    handle.user_message("again".into()).await.unwrap();
    wait_for_turn_completed(&mut events).await;
    assert_eq!(
        engine.state.lock().unwrap().as_slice(),
        &["again".to_string()],
        "the second body must commit after the rolled-back failure"
    );
    handle.stop().await.unwrap();
}

/// The accepted-event audit fails after the message was applied: the turn
/// must be fenced before any further mutation.
#[tokio::test]
async fn user_message_accepted_audit_failure_fences_the_runtime() {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailUserMessageAcceptedJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let error = handle.user_message("hello".into()).await.unwrap_err();
    assert!(
        matches!(error, AgentError::RecoveryRequired(_)),
        "the applied-but-unaudited message must fence: {error}"
    );
    let error = handle.user_message("again".into()).await.unwrap_err();
    assert!(
        matches!(error, AgentError::RecoveryRequired(_)),
        "a fenced runtime must reject further mutation: {error}"
    );
    handle.stop().await.unwrap();
}

/// The maintained-event audit fails after the accepted event landed: the
/// runtime must still fence because the maintenance state changed without
/// its audit record.
#[tokio::test]
async fn user_input_maintained_audit_failure_fences_the_runtime() {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(BodyTrackingEngine::default()),
        Arc::new(SilentModel),
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailUserInputMaintainedJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();

    let error = handle.user_message("hello".into()).await.unwrap_err();
    assert!(
        matches!(error, AgentError::RecoveryRequired(_)),
        "the maintenance state must fence without its audit event: {error}"
    );
    let error = handle.user_message("again".into()).await.unwrap_err();
    assert!(
        matches!(error, AgentError::RecoveryRequired(_)),
        "a fenced runtime must reject further mutation: {error}"
    );
    handle.stop().await.unwrap();
}

/// Wire an instance whose service registry sees a live capability so the
/// safe-point capture handshake has a real generation plane.
async fn instance_with_registry(
    engine: Arc<BodyTrackingEngine>,
    registry: Arc<CapabilityRegistry>,
) -> RuntimeInstance {
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(TestToolDispatcher),
        registry,
    ));
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .expect("tool module registers");
    let services = RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine,
        Arc::new(SilentModel),
        dispatcher,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    );
    host.start().await.expect("test module host starts");
    RuntimeInstance::spawn(host, services)
}

/// One checkpoint assembly runs the Checkpoint maintenance pass exactly
/// once, even when the generation-stability retry re-enters the capture.
#[tokio::test]
async fn checkpoint_assembly_runs_checkpoint_maintenance_once() {
    let engine = Arc::new(BodyTrackingEngine::default());
    engine.slow_checkpoint_ms.store(120, Ordering::SeqCst);
    let registry = Arc::new(CapabilityRegistry::new());
    registry
        .register(Arc::new(TestCapability::new("demo")))
        .expect("capability registers");
    let instance = instance_with_registry(engine.clone(), registry.clone()).await;
    instance.start().await.unwrap();

    // Flip the surface mid-capture: the first attempt observes a stale
    // generation and must retry against a stable one.
    let racing = registry.clone();
    let racer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        racing
            .disable("demo")
            .await
            .expect("disable bumps the generation");
    });
    instance.checkpoint().await.unwrap();
    racer.await.unwrap();

    assert_eq!(
        engine.checkpoint_maintains.load(Ordering::SeqCst),
        1,
        "the retried assembly must not repeat logical maintenance"
    );

    // A second, stable assembly still performs exactly one more pass.
    instance.checkpoint().await.unwrap();
    assert_eq!(
        engine.checkpoint_maintains.load(Ordering::SeqCst),
        2,
        "each subsequent assembly performs exactly one pass"
    );
    instance.shutdown().await.unwrap();
}

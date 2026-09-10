//! W04: each turn maintenance phase runs as a cancellable operation.
//! A blocked maintenance pass must not keep the actor from answering
//! cancel_turn, and aborting the pass must be the engine's safe failure.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ContextDiagnostics, ContextEngine, ContextGcReport, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest,
    ModelTransport, RuntimeEvent, ScopeId, ScopeKind,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};

/// Engine that gates the BeforeModel maintenance pass. Dropping the gated
/// future marks `aborted`, proving cancellation killed the in-flight pass
/// instead of waiting it out.
#[derive(Debug)]
struct GatedMaintenanceEngine {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    aborted: Arc<AtomicBool>,
    trigger: ContextMaintenanceTrigger,
    enabled: AtomicBool,
    fail_restore: AtomicBool,
    bodies: Mutex<Vec<String>>,
    restores: AtomicUsize,
    gc_calls: AtomicUsize,
}

impl GatedMaintenanceEngine {
    fn new(trigger: ContextMaintenanceTrigger) -> Self {
        Self {
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            aborted: Arc::new(AtomicBool::new(false)),
            trigger,
            enabled: AtomicBool::new(true),
            fail_restore: AtomicBool::new(false),
            bodies: Mutex::new(Vec::new()),
            restores: AtomicUsize::new(0),
            gc_calls: AtomicUsize::new(0),
        }
    }

    async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.entered.notified())
            .await
            .expect("maintenance did not enter its gate");
    }
}

struct AbortGuard {
    aborted: Arc<AtomicBool>,
    completed: bool,
}
impl Drop for AbortGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.aborted.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl ContextEngine for GatedMaintenanceEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let body = match ingress {
            ContextIngress::UserMessage { content } => Some(format!("user:{content}")),
            ContextIngress::AssistantMessage { content } => Some(format!("assistant:{content}")),
            ContextIngress::ToolObservation { output, .. } => {
                Some(format!("tool:{}", output.call_id))
            }
            _ => None,
        };
        if let Some(body) = body {
            self.bodies.lock().unwrap().push(body);
        }
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        if trigger == self.trigger && self.enabled.load(Ordering::SeqCst) {
            let mut guard = AbortGuard {
                aborted: self.aborted.clone(),
                completed: false,
            };
            self.entered.notify_one();
            self.release.notified().await;
            guard.completed = true;
        }
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: Default::default(),
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
        Ok(serde_json::json!(*self.bodies.lock().unwrap()))
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.fail_restore.load(Ordering::SeqCst) {
            return Err(AgentError::Context("injected rollback failure".into()));
        }
        *self.bodies.lock().unwrap() = serde_json::from_value(data).unwrap();
        Ok(())
    }
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        self.gc_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContextGcReport::default())
    }
}

/// Records every model call; the cancelled turn must never reach it.
#[derive(Debug, Default)]
struct RecordingModel {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput {
            content: "should never happen".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn cancel_turn_answers_while_before_model_maintenance_is_blocked() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::BeforeModel,
    ));
    let entered = engine.entered.clone();
    let aborted = engine.aborted.clone();
    let model = Arc::new(RecordingModel::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine,
        model.clone() as Arc<dyn ModelTransport>,
        Arc::new(super::harness::TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(services);
    handle.start().await.unwrap();
    handle
        .set_focus("cancel during maintenance".into())
        .await
        .unwrap();

    let mut events = handle.subscribe();
    handle
        .user_message("long running maintenance first".into())
        .await
        .unwrap();

    // The spawned maintenance pass is in flight and gated: the round has
    // not reached the model.
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("before-model maintenance did not start");

    // The acceptance: the cancel receipt arrives while the pass is still
    // blocked — the actor loop was never held by the maintenance await.
    let ack = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel_turn was stuck behind the blocked maintenance pass")
        .unwrap();
    assert!(matches!(
        ack,
        agent_contracts::TurnCancelAck::Cancelled { .. }
    ));

    // Aborting the spawned pass is the engine's safe failure: the gated
    // future was dropped at its await point.
    assert!(
        aborted.load(Ordering::SeqCst),
        "cancel acknowledgement must follow future drop"
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "a cancelled turn must never reach the model"
    );

    // The turn terminal is the typed cancellation, not a model round.
    let mut saw_cancelled = false;
    let mut saw_model_started = false;
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            RuntimeEvent::TurnCancelled { .. } => saw_cancelled = true,
            RuntimeEvent::ModelStarted { .. } => saw_model_started = true,
            _ => {}
        }
    }
    assert!(saw_cancelled, "the turn must finalize as cancelled");
    assert!(!saw_model_started, "no model round may start after cancel");
    handle.stop().await.unwrap();
}

async fn input_runtime(
    engine: Arc<GatedMaintenanceEngine>,
    model: Arc<RecordingModel>,
) -> agent_runtime::RuntimeHandle {
    let (handle, _) = spawn_runtime(Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine,
        model,
        Arc::new(super::harness::TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    )));
    handle.start().await.unwrap();
    handle
        .set_focus("maintenance admission".into())
        .await
        .unwrap();
    handle
}

async fn completed(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::TurnCompleted
            ) {
                break;
            }
        }
    })
    .await
    .expect("turn did not complete");
}

#[tokio::test]
async fn user_input_cancel_rolls_back_before_admitting_queued_input() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::UserInput,
    ));
    let model = Arc::new(RecordingModel::default());
    let handle = input_runtime(engine.clone(), model.clone()).await;
    let mut events = handle.subscribe();
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move { handle.user_message("cancelled body".into()).await }
    });
    engine.wait_entered().await;
    assert!(
        !waiting.is_finished(),
        "input must await its transaction receipt"
    );
    handle.user_message("queued body".into()).await.unwrap();
    engine.enabled.store(false, Ordering::SeqCst);
    let ack = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel waited on UserInput")
        .unwrap();
    assert!(matches!(
        ack,
        agent_contracts::TurnCancelAck::Cancelled { .. }
    ));
    assert!(engine.aborted.load(Ordering::SeqCst));
    assert!(matches!(waiting.await.unwrap(), Err(AgentError::Cancelled)));
    completed(&mut events).await;
    let bodies = engine.bodies.lock().unwrap().clone();
    assert!(!bodies.iter().any(|body| body.contains("cancelled body")));
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.as_str() == "user:queued body")
            .count(),
        1
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(engine.restores.load(Ordering::SeqCst), 1);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn user_input_cancel_rollback_failure_is_recovery_required() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::UserInput,
    ));
    engine.fail_restore.store(true, Ordering::SeqCst);
    let model = Arc::new(RecordingModel::default());
    let handle = input_runtime(engine.clone(), model.clone()).await;
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move { handle.user_message("unrestorable body".into()).await }
    });
    engine.wait_entered().await;
    let error = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, AgentError::RecoveryRequired(_)));
    assert!(matches!(
        waiting.await.unwrap(),
        Err(AgentError::RecoveryRequired(_))
    ));
    assert!(engine.aborted.load(Ordering::SeqCst));
    assert!(matches!(
        handle.user_message("must be refused".into()).await,
        Err(AgentError::RecoveryRequired(_))
    ));
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn stop_during_user_input_joins_maintenance_and_restores_the_body() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::UserInput,
    ));
    let handle = input_runtime(engine.clone(), Arc::new(RecordingModel::default())).await;
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move { handle.user_message("abandoned input".into()).await }
    });
    engine.wait_entered().await;
    tokio::time::timeout(Duration::from_secs(2), handle.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(waiting.await.unwrap(), Err(AgentError::Cancelled)));
    assert!(engine.aborted.load(Ordering::SeqCst));
    assert!(engine.bodies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn continuation_cancel_does_not_reingest_or_replace_the_directive() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::UserInput,
    ));
    engine.enabled.store(false, Ordering::SeqCst);
    let handle = input_runtime(engine.clone(), Arc::new(RecordingModel::default())).await;
    let mut events = handle.subscribe();
    handle
        .user_message("original directive".into())
        .await
        .unwrap();
    completed(&mut events).await;
    let before = engine.bodies.lock().unwrap().clone();
    engine.enabled.store(true, Ordering::SeqCst);
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move { handle.continue_active_task().await }
    });
    engine.wait_entered().await;
    tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(waiting.await.unwrap(), Err(AgentError::Cancelled)));
    assert_eq!(*engine.bodies.lock().unwrap(), before);
    assert_eq!(engine.restores.load(Ordering::SeqCst), 0);
    engine.enabled.store(false, Ordering::SeqCst);
    handle.continue_active_task().await.unwrap();
    completed(&mut events).await;
    assert_eq!(
        engine
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.as_str() == "user:original directive")
            .count(),
        1
    );
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn cancelled_work_submission_has_no_success_receipt_and_can_retry() {
    let engine = Arc::new(GatedMaintenanceEngine::new(
        ContextMaintenanceTrigger::UserInput,
    ));
    let handle = input_runtime(engine.clone(), Arc::new(RecordingModel::default())).await;
    let mut events = handle.subscribe();
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .start_work("bounded work".into(), "request-1".into())
                .await
        }
    });
    engine.wait_entered().await;
    assert!(!waiting.is_finished());
    tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(waiting.await.unwrap(), Err(AgentError::Cancelled)));
    engine.enabled.store(false, Ordering::SeqCst);
    let receipt = handle
        .start_work("bounded work".into(), "request-1".into())
        .await
        .unwrap();
    assert_eq!(
        receipt.disposition,
        agent_runtime::WorkSubmissionDisposition::Accepted
    );
    completed(&mut events).await;
    let repeated = handle
        .start_work("bounded work".into(), "request-1".into())
        .await
        .unwrap();
    assert_eq!(
        repeated.disposition,
        agent_runtime::WorkSubmissionDisposition::AlreadyAccepted
    );
    assert_eq!(
        engine
            .bodies
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.as_str() == "user:bounded work")
            .count(),
        1
    );
    handle.stop().await.unwrap();
}

#[derive(Default)]
struct CommitJournal {
    pending: Mutex<Vec<RuntimeEvent>>,
    durable: Mutex<Vec<RuntimeEvent>>,
}

#[async_trait::async_trait]
impl agent_contracts::EventJournal for CommitJournal {
    async fn append(&self, envelope: &agent_contracts::RuntimeEventEnvelope) -> AgentResult<()> {
        self.pending.lock().unwrap().push(envelope.event.clone());
        Ok(())
    }
    async fn flush(&self) -> AgentResult<()> {
        self.durable
            .lock()
            .unwrap()
            .extend(self.pending.lock().unwrap().drain(..));
        Ok(())
    }
}

async fn cancel_committing_maintenance(trigger: ContextMaintenanceTrigger, expected_phase: &str) {
    let engine = Arc::new(GatedMaintenanceEngine::new(trigger));
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(super::harness::TwoRoundToolModel::default());
    let journal = Arc::new(CommitJournal::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine.clone(),
        model.clone(),
        Arc::new(super::effects::EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
        Some(journal.clone()),
    ));
    let (handle, _) = spawn_runtime(services);
    handle.start().await.unwrap();
    handle.user_message("apply once".into()).await.unwrap();
    engine.wait_entered().await;
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    let before = engine.bodies.lock().unwrap().clone();
    let error = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel waited on commit maintenance")
        .unwrap_err();
    assert!(matches!(error, AgentError::RecoveryRequired(_)));
    assert!(error.to_string().contains(expected_phase), "{error}");
    assert!(
        engine.aborted.load(Ordering::SeqCst),
        "receipt precedes engine future drop"
    );
    assert_eq!(*engine.bodies.lock().unwrap(), before);
    assert_eq!(
        engine.restores.load(Ordering::SeqCst),
        0,
        "post-effect state must never roll back"
    );
    assert_eq!(
        engine.gc_calls.load(Ordering::SeqCst),
        0,
        "later commit phases must stop"
    );
    let durable = journal.durable.lock().unwrap().clone();
    assert!(durable.iter().any(|event| matches!(event, RuntimeEvent::TurnCommitFailed { phase, .. } if phase == expected_phase)));
    assert!(
        durable
            .iter()
            .any(|event| matches!(event, RuntimeEvent::RecoveryRequired))
    );
    assert!(!durable.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnCompleted | RuntimeEvent::TurnCancelled { .. }
    )));
    let operation_id = durable
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::OperationAccepted { snapshot } => Some(snapshot.identity.operation_id),
            _ => None,
        })
        .expect("Core admitted the effect operation");
    let agent_contracts::OperationQueryResult::Found { snapshot } =
        handle.query_operation(operation_id).await.unwrap()
    else {
        panic!("missing effect truth")
    };
    assert!(matches!(
        snapshot.state,
        agent_contracts::OperationState::Terminal {
            terminal: agent_contracts::OperationTerminal::Applied { .. },
            ..
        }
    ));
    engine.enabled.store(false, Ordering::SeqCst);
    engine.release.notify_one();
    assert!(matches!(
        handle.user_message("do not replay".into()).await,
        Err(AgentError::RecoveryRequired(_))
    ));
    assert_eq!(model.rounds.load(Ordering::SeqCst), 2);
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    assert_eq!(rolled_back.load(Ordering::SeqCst), 0);
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn after_tool_cancel_fences_commit_and_preserves_applied_effect() {
    cancel_committing_maintenance(ContextMaintenanceTrigger::AfterTool, "after_tool_maintain")
        .await;
}

#[tokio::test]
async fn after_model_cancel_fences_commit_and_preserves_applied_effect() {
    cancel_committing_maintenance(
        ContextMaintenanceTrigger::AfterModel,
        "after_model_maintain",
    )
    .await;
}

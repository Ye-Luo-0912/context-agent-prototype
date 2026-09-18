//! A settled provider failure must preserve the last accepted directive in
//! the same durable continuation plane used by a deliberate budget stop.
use crate::harness::TestToolDispatcher;
use agent_contracts::{
    AgentError, AgentResult, ModelCapabilities, ModelOutput, ModelProtocolErrorKind, ModelRequest,
    ModelTransport, RuntimeEvent, RuntimeEventEnvelope,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    CheckpointStore, ModuleHost, RuntimeCheckpoint, RuntimeInstance, RuntimeServices,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const OLD: &str = "initial directive: inspect the queue";
const NEW: &str = concat!(
    "CORRECTION-917: reject duplicate ids and unstripped keys; retain this feedback. ",
    "Preserve the original queue-scoped deduplication semantics and the existing transaction boundary. ",
    "Do not accept a tuple by silently converting it to a list, and keep the immutable public tests intact. ",
    "The correction must survive independently of whether this turn changed a file or only inspected it. ",
    "BEYOND-PREVIEW-917: retain this final instruction and report any remaining limitations."
);

struct FailingModel {
    fail: AtomicBool,
    requests: Mutex<Vec<ModelRequest>>,
    error: fn() -> AgentError,
    input_budget: bool,
    read_first: bool,
}
impl Default for FailingModel {
    fn default() -> Self {
        Self {
            fail: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
            input_budget: false,
            read_first: false,
            error: || AgentError::ModelProtocol {
                kind: ModelProtocolErrorKind::MalformedToolCall,
                message: "injected malformed arguments after accepting feedback".into(),
            },
        }
    }
}
#[async_trait::async_trait]
impl ModelTransport for FailingModel {
    fn capabilities(&self) -> ModelCapabilities {
        if self.input_budget && self.fail.load(Ordering::SeqCst) {
            ModelCapabilities {
                context_window: Some(1),
                ..Default::default()
            }
        } else {
            ModelCapabilities::default()
        }
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(request);
        if self.read_first && self.requests.lock().unwrap().len() == 1 {
            return Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![agent_contracts::ToolCall {
                    id: "probe".into(),
                    name: "fs.read".into(),
                    arguments: serde_json::json!({}),
                }],
                usage: Default::default(),
            });
        }
        if self.fail.load(Ordering::SeqCst) {
            Err(AgentError::failed_with_usage(
                agent_contracts::ModelUsage {
                    input_tokens: Some(101),
                    output_tokens: Some(7),
                    ..Default::default()
                },
                (self.error)(),
            ))
        } else {
            Ok(ModelOutput {
                content: "noted".into(),
                tool_calls: vec![],
                usage: Default::default(),
            })
        }
    }
}

async fn instance(root: &std::path::Path, model: Arc<FailingModel>) -> RuntimeInstance {
    instance_with_context(
        root,
        model,
        Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
    )
    .await
}

async fn instance_with_context(
    root: &std::path::Path,
    model: Arc<FailingModel>,
    context: Arc<dyn agent_contracts::ContextEngine>,
) -> RuntimeInstance {
    let tools: Arc<dyn agent_contracts::ToolDispatcher> = if model.read_first {
        Arc::new(ReadOnlyProbe)
    } else {
        Arc::new(TestToolDispatcher)
    };
    let workspace = Arc::new(agent_workspace::Workspace::open(root).await.unwrap());
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces"))
            .await
            .unwrap(),
    );
    let services = RuntimeServices::try_new(
        CoreAuthorityConfig::default(),
        context,
        model,
        tools,
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal),
        agent_runtime::AuthorityRecoveryServices::new(
            Arc::new(
                agent_storage::FileOperationJournal::open(root.join("operations.jsonl"))
                    .unwrap()
                    .0,
            ),
            None,
        ),
    )
    .unwrap()
    .with_artifact_workspace(workspace);
    let mut host = ModuleHost::new();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services);
    instance.start().await.unwrap();
    instance
}

#[derive(Debug)]
struct ReadOnlyProbe;
#[async_trait::async_trait]
impl agent_contracts::ToolDispatcher for ReadOnlyProbe {
    fn specs(&self) -> Vec<agent_contracts::ToolSpec> {
        vec![agent_contracts::ToolSpec {
            name: "fs.read".into(),
            description: "read-only fixture".into(),
            input_schema: serde_json::json!({"type":"object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![],
        }]
    }
    async fn execute(
        &self,
        request: agent_contracts::ToolExecutionRequest,
    ) -> AgentResult<agent_contracts::ToolOutcome> {
        Ok(agent_contracts::ToolOutcome::Value(
            agent_contracts::ToolOutput {
                call_id: request.call.id,
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: "observed fixture".into(),
                artifact_ref: None,
                metadata: serde_json::json!({"path":"probe.txt","revision":"v1"}),
            },
        ))
    }
}

async fn terminal(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<RuntimeEvent> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut seen = Vec::new();
        loop {
            let event = events.recv().await.unwrap().event;
            let done = matches!(
                event,
                RuntimeEvent::TurnCompleted | RuntimeEvent::TurnFailed { .. }
            );
            seen.push(event);
            if done {
                return seen;
            }
        }
    })
    .await
    .expect("turn terminal must arrive")
}

#[tokio::test]
async fn model_failure_preserves_new_feedback_before_terminal_and_cold_restore() {
    restore_case(Arc::new(FailingModel::default())).await;
}

#[tokio::test]
async fn transport_failure_preserves_feedback_without_replaying_the_call() {
    restore_case(Arc::new(FailingModel {
        error: || AgentError::Transport {
            retryable: true,
            message: "injected transport failure".into(),
        },
        ..Default::default()
    }))
    .await;
}

#[tokio::test]
async fn output_limit_preserves_feedback_without_replaying_the_call() {
    restore_case(Arc::new(FailingModel {
        error: || AgentError::ModelOutputLimit {
            reason: "injected output budget failure".into(),
        },
        ..Default::default()
    }))
    .await;
}

#[tokio::test]
async fn input_budget_refusal_preserves_feedback_before_any_provider_call() {
    restore_case(Arc::new(FailingModel {
        input_budget: true,
        ..Default::default()
    }))
    .await;
}

async fn restore_case(model: Arc<FailingModel>) {
    let dir = tempfile::tempdir().unwrap();
    let first = instance(dir.path(), model.clone()).await;
    let handle = first.handle();
    let mut events = handle.subscribe();
    handle.user_message(OLD.into()).await.unwrap();
    terminal(&mut events).await;
    let old = first.checkpoint().await.unwrap();
    let task_id = old.current_task_id.unwrap();
    let store = CheckpointStore::new(dir.path().join(".focus-agent/checkpoints"));
    store
        .write_atomic(&serde_json::to_vec(&old).unwrap())
        .await
        .unwrap();

    model.fail.store(true, Ordering::SeqCst);
    handle.user_message(NEW.into()).await.unwrap();
    let seen = terminal(&mut events).await;
    assert_eq!(
        model.requests.lock().unwrap().len(),
        if model.input_budget { 1 } else { 2 }
    );
    if !model.input_budget {
        assert!(
            model
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.content.contains(NEW))
        );
    }
    let durable = seen
        .iter()
        .position(|e| matches!(e, RuntimeEvent::CheckpointDurable { .. }))
        .expect("a provider failure must not leave latest pointing at the older directive");
    let used = seen.iter().position(|e| {
        matches!(
            e,
            RuntimeEvent::ModelUsed {
                input_tokens: 101,
                output_tokens: 7,
                ..
            }
        )
    });
    let failed = seen
        .iter()
        .position(|e| matches!(e, RuntimeEvent::TurnFailed { .. }))
        .unwrap();
    if model.input_budget {
        assert!(used.is_none());
    } else {
        assert!(used.unwrap() < durable);
    }
    assert!(durable < failed);
    assert!(!seen.iter().any(|e| matches!(
        e,
        RuntimeEvent::RecoveryRequired
            | RuntimeEvent::TurnCompleted
            | RuntimeEvent::TaskCompleted { .. }
    )));
    let latest = store.list(1).await.unwrap().remove(0);
    let restored: RuntimeCheckpoint =
        serde_json::from_slice(&store.load_verified(&latest.artifact).await.unwrap()).unwrap();
    let saved = restored
        .tasks
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .unwrap();
    assert!(NEW.len() > agent_contracts::USER_INPUT_PREVIEW_CHARS);
    assert_eq!(
        saved.current_directive.as_ref().unwrap().input.digest,
        Some(agent_contracts::ContentDigest::sha256_bytes(NEW.as_bytes()).to_string()),
        "restore must retain the full input identity, not only its bounded preview"
    );
    let directive_epoch = saved.resume.directive_revision;
    first.shutdown().await.unwrap();

    let recorder = Arc::new(FailingModel::default());
    let second = instance(dir.path(), recorder.clone()).await;
    second.restore(restored).await.unwrap();
    let mut events = second.handle().subscribe();
    second.handle().continue_active_task().await.unwrap();
    let resumed = terminal(&mut events).await;
    assert!(
        recorder.requests.lock().unwrap()[0]
            .messages
            .iter()
            .any(|m| m.content.contains(NEW))
    );
    let checkpoint = second.checkpoint().await.unwrap();
    assert_eq!(checkpoint.current_task_id, Some(task_id));
    let task = checkpoint
        .tasks
        .tasks
        .iter()
        .find(|t| t.id == task_id)
        .unwrap();
    assert_eq!(
        task.resume.directive_revision, directive_epoch,
        "continuation must not re-admit feedback"
    );
    assert!(!resumed.iter().any(|e| matches!(e, RuntimeEvent::UserMessageAccepted { input } if input.kind == agent_contracts::InputKind::Dialogue)));
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_resume_write_fences_instead_of_claiming_durability() {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(FailingModel::default());
    model.fail.store(true, Ordering::SeqCst);
    let runtime = instance(dir.path(), model).await;
    std::fs::write(
        dir.path().join(".focus-agent/checkpoints"),
        b"blocked store",
    )
    .unwrap();
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle.user_message(NEW.into()).await.unwrap();
    let seen = terminal(&mut events).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, RuntimeEvent::CheckpointWriteFailed { .. }))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, RuntimeEvent::RecoveryRequired))
    );
    assert!(!seen.iter().any(|e| matches!(
        e,
        RuntimeEvent::CheckpointDurable { .. } | RuntimeEvent::TaskCompleted { .. }
    )));
    assert!(handle.continue_active_task().await.is_err());
    let _ = runtime.shutdown().await; // the intentionally broken store remains fenced
}

struct CheckpointGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    fail: bool,
}
#[async_trait::async_trait]
impl agent_contracts::ContextEngine for CheckpointGate {
    async fn ingest(&self, _: agent_contracts::ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: agent_contracts::ContextMaintenanceTrigger,
    ) -> AgentResult<agent_contracts::ContextMaintenanceReport> {
        if trigger == agent_contracts::ContextMaintenanceTrigger::Checkpoint {
            self.entered.notify_one();
            if self.fail {
                return Err(AgentError::Context(
                    "injected checkpoint maintenance failure".into(),
                ));
            }
            let _permit = self.release.acquire().await.unwrap();
        }
        Ok(Default::default())
    }
    async fn materialize(
        &self,
        _: agent_contracts::ContextQuery,
    ) -> AgentResult<agent_contracts::MaterializedContext> {
        Ok(Default::default())
    }
    async fn open_scope(
        &self,
        _: agent_contracts::ScopeKind,
        _: Option<agent_contracts::ScopeId>,
    ) -> AgentResult<agent_contracts::ScopeId> {
        Ok(agent_contracts::ScopeId::new())
    }
    async fn close_scope(
        &self,
        _: agent_contracts::ScopeId,
    ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
        Ok(vec![])
    }
    async fn diagnostics(&self) -> AgentResult<agent_contracts::ContextDiagnostics> {
        Ok(Default::default())
    }
    async fn inspect(&self, _: usize) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
        Ok(vec![])
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn failed_checkpoint_maintenance_never_publishes_a_resumable_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(CheckpointGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
        fail: true,
    });
    let model = Arc::new(FailingModel::default());
    model.fail.store(true, Ordering::SeqCst);
    let runtime = instance_with_context(dir.path(), model, context).await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle.user_message(NEW.into()).await.unwrap();
    let seen = terminal(&mut events).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, RuntimeEvent::CheckpointWriteFailed { .. }))
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, RuntimeEvent::RecoveryRequired))
    );
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, RuntimeEvent::CheckpointDurable { .. }))
    );
    assert!(handle.continue_active_task().await.is_err());
    let _ = runtime.shutdown().await;
}

#[tokio::test]
async fn failed_turn_checkpoint_wait_keeps_status_and_cancellation_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(CheckpointGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let model = Arc::new(FailingModel::default());
    model.fail.store(true, Ordering::SeqCst);
    let runtime = instance_with_context(dir.path(), model, context.clone()).await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle.user_message(NEW.into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), context.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .expect("status must stay responsive")
        .unwrap();
    let ack = tokio::time::timeout(Duration::from_secs(2), handle.cancel_turn())
        .await
        .expect("cancel must not await checkpoint maintenance")
        .unwrap();
    assert!(matches!(
        ack,
        agent_contracts::TurnCancelAck::Cancelled { .. }
    ));
    context.release.add_permits(2);
    // This explicit barrier also joins the detached prepare. Its stale relay
    // completion must never resurrect the failed-turn tail after cancellation.
    tokio::time::timeout(Duration::from_secs(5), runtime.checkpoint())
        .await
        .unwrap()
        .unwrap();
    let mut cancelled = false;
    while let Ok(envelope) = events.try_recv() {
        assert!(!matches!(
            envelope.event,
            RuntimeEvent::TurnCompleted | RuntimeEvent::TurnFailed { .. }
        ));
        cancelled |= matches!(envelope.event, RuntimeEvent::TurnCancelled { .. });
    }
    assert!(cancelled);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failure_drains_an_older_prepare_before_committing_its_own_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(CheckpointGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let model = Arc::new(FailingModel {
        read_first: true,
        ..Default::default()
    });
    model.fail.store(true, Ordering::SeqCst);
    let runtime = instance_with_context(dir.path(), model, context.clone()).await;
    let handle = runtime.handle();
    handle.set_focus(OLD.into()).await.unwrap();
    let task_id = handle.list_tasks().await.unwrap()[0].id;
    handle
        .patch_task_anchor(
            task_id,
            0,
            agent_runtime::AnchorPatch {
                next_action: Some("inspect the fixture".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut events = handle.subscribe();
    handle.user_message(NEW.into()).await.unwrap();
    // The read settles the anchor debt into a gated prepare; the next model
    // call fails while that prepare is still outstanding.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::Failure { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .unwrap()
        .unwrap();
    context.release.add_permits(2);
    let seen = terminal(&mut events).await;
    let sequences: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::CheckpointDurable { sequence, .. } => Some(*sequence),
            _ => None,
        })
        .collect();
    assert_eq!(
        sequences.len(),
        2,
        "both the prior debt and failed-turn snapshot must land"
    );
    assert!(sequences[0] < sequences[1]);
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, RuntimeEvent::RecoveryRequired))
    );
    runtime.shutdown().await.unwrap();
}

/// Endurance F12: retain a real completion GC owner while a later task's
/// failed-turn checkpoint is waiting on the real engine's maintenance.
/// The gates control I/O timing only; all state is admitted through public APIs.
struct OccupiedBoundaryContext {
    inner: SimpleContextEngine,
    gc_entered: tokio::sync::Notify,
    gc_release: tokio::sync::Semaphore,
    checkpoint_entered: tokio::sync::Notify,
    checkpoint_release: tokio::sync::Semaphore,
    hold_gc: AtomicBool,
    hold_checkpoint: AtomicBool,
}

#[async_trait::async_trait]
impl agent_contracts::ContextEngine for OccupiedBoundaryContext {
    async fn ingest(&self, value: agent_contracts::ContextIngress) -> AgentResult<()> {
        self.inner.ingest(value).await
    }
    async fn maintain(
        &self,
        trigger: agent_contracts::ContextMaintenanceTrigger,
    ) -> AgentResult<agent_contracts::ContextMaintenanceReport> {
        if trigger == agent_contracts::ContextMaintenanceTrigger::Checkpoint
            && self.hold_checkpoint.load(Ordering::SeqCst)
        {
            self.checkpoint_entered.notify_one();
            let _permit = self.checkpoint_release.acquire().await.unwrap();
        }
        self.inner.maintain(trigger).await
    }
    async fn gc(&self) -> AgentResult<agent_contracts::ContextGcReport> {
        if self.hold_gc.load(Ordering::SeqCst) {
            self.gc_entered.notify_one();
            let _permit = self.gc_release.acquire().await.unwrap();
        }
        self.inner.gc().await
    }
    async fn materialize(
        &self,
        query: agent_contracts::ContextQuery,
    ) -> AgentResult<agent_contracts::MaterializedContext> {
        self.inner.materialize(query).await
    }
    async fn open_scope(
        &self,
        kind: agent_contracts::ScopeKind,
        parent: Option<agent_contracts::ScopeId>,
    ) -> AgentResult<agent_contracts::ScopeId> {
        self.inner.open_scope(kind, parent).await
    }
    async fn close_scope(
        &self,
        id: agent_contracts::ScopeId,
    ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
        self.inner.close_scope(id).await
    }
    async fn diagnostics(&self) -> AgentResult<agent_contracts::ContextDiagnostics> {
        self.inner.diagnostics().await
    }
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
        self.inner.inspect(limit).await
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        self.inner.checkpoint().await
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        self.inner.restore(data).await
    }
}

#[tokio::test]
async fn endurance_occupied_completion_gc_keeps_failed_turn_controls_responsive() {
    occupied_boundary_probe(true).await;
}

#[tokio::test]
#[ignore = "endurance F12 matched free-lane control; run explicitly"]
async fn endurance_free_completion_lane_keeps_failed_turn_controls_responsive() {
    occupied_boundary_probe(false).await;
}

async fn occupied_boundary_probe(occupy_gc: bool) {
    let dir = tempfile::tempdir().unwrap();
    let context = Arc::new(OccupiedBoundaryContext {
        inner: SimpleContextEngine::new(SimpleContextConfig::default()),
        gc_entered: Default::default(),
        gc_release: tokio::sync::Semaphore::new(0),
        checkpoint_entered: Default::default(),
        checkpoint_release: tokio::sync::Semaphore::new(0),
        hold_gc: AtomicBool::new(true),
        hold_checkpoint: AtomicBool::new(false),
    });
    let model = Arc::new(FailingModel::default());
    let runtime = instance_with_context(dir.path(), model.clone(), context.clone()).await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle.set_focus("old completed task".into()).await.unwrap();
    handle
        .complete_current_task("operator closes old task".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), context.gc_entered.notified())
        .await
        .unwrap();
    if !occupy_gc {
        context.hold_gc.store(false, Ordering::SeqCst);
        context.gc_release.add_permits(8);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    events.recv().await.unwrap().event,
                    RuntimeEvent::StorageGc { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    // This confirms that an occupied boundary alone does not block the actor.
    tokio::time::timeout(Duration::from_secs(2), handle.status_snapshot())
        .await
        .unwrap()
        .unwrap();
    handle
        .set_focus("new task with failure".into())
        .await
        .unwrap();
    context.hold_checkpoint.store(true, Ordering::SeqCst);
    model.fail.store(true, Ordering::SeqCst);
    handle.user_message(NEW.into()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        context.checkpoint_entered.notified(),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(10), handle.status_snapshot()).await;
    let status_ms = started.elapsed().as_millis();
    let started = std::time::Instant::now();
    let cancel = tokio::time::timeout(Duration::from_secs(10), handle.cancel_turn()).await;
    let cancel_ms = started.elapsed().as_millis();
    let mutation_while_pending = handle
        .set_focus("must wait for failed-turn checkpoint".into())
        .await;
    if occupy_gc {
        assert!(
            mutation_while_pending.is_err(),
            "new mutation must not overtake the parked failed-turn checkpoint"
        );
    }
    // Release ONLY checkpoint maintenance first. In the occupied case the GC
    // owner is still held: a recovered status proves which wait blocked dispatch.
    context.hold_checkpoint.store(false, Ordering::SeqCst);
    context.checkpoint_release.add_permits(8);
    let status_after_checkpoint =
        tokio::time::timeout(Duration::from_secs(10), handle.status_snapshot()).await;
    context.hold_gc.store(false, Ordering::SeqCst);
    context.gc_release.add_permits(8);
    let shutdown = tokio::time::timeout(Duration::from_secs(15), runtime.shutdown()).await;
    let mut seen = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        seen.push(envelope.event);
    }
    let receipt = serde_json::json!({
        "case": "F12", "engine": "SimpleContextEngine with timing gates",
        "gc_owner_observed": true, "gc_occupied_at_failure": occupy_gc,
        "checkpoint_wait_observed": true,
        "status_after_only_checkpoint_release": format!("{status_after_checkpoint:?}"),
        "status_completed": status.is_ok(), "status_ms": status_ms,
        "cancel_completed": cancel.is_ok(), "cancel_ms": cancel_ms,
        "status_result": format!("{status:?}"), "cancel_result": format!("{cancel:?}"),
        "shutdown": format!("{shutdown:?}"), "events": seen,
    });
    if let Ok(path) = std::env::var("ENDURANCE_F12_RECEIPT") {
        std::fs::write(path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    }
    eprintln!(
        "F12 status={status_ms}ms cancel={cancel_ms}ms status_ok={} cancel_ok={}",
        status.is_ok(),
        cancel.is_ok()
    );
    assert!(shutdown.is_ok_and(|r| r.is_ok()), "probe must cleanly join");
    assert!(
        status_after_checkpoint.is_ok_and(|r| r.is_ok()),
        "checkpoint-only release must restore command dispatch even with GC still held"
    );
    assert!(
        status.is_ok_and(|r| r.is_ok()),
        "F12: occupied GC owner must not block status behind failed-turn checkpoint maintenance"
    );
    assert!(
        cancel.is_ok_and(|r| r.is_ok()),
        "F12: cancel must remain serviceable"
    );
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, RuntimeEvent::TurnCancelled { .. }))
            .count(),
        1,
        "the cancelled turn must have exactly one durable cancellation terminal"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFailed { .. })),
        "a cancelled turn must not publish a late failure terminal"
    );
}

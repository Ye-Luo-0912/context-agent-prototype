use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, Capability, CapabilityInvocationContext, CapabilityLifecycle,
    CapabilityManifest, CapabilityOutcome, CapabilityStatus, CapabilityTransport,
    ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    Effect, EffectDurability, EffectReceipt, MaterializedContext, ModelCapabilities, ModelOutput,
    ModelRequest, ModelTransport, RuntimeEvent, ScopeId, ScopeKind, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
};

use agent_core::PolicyApprovalGate;
use agent_runtime::{CapabilityAwareDispatcher, CapabilityRegistry, RuntimeHandle};
use serde_json::json;

use crate::harness::*;

// Effect commit: a tool's computation is separate from its side-effect
// commit. The actor commits after the generation fence and rolls back a
// stale operation's prepared effect.
// ---------------------------------------------------------------------------

/// A staged effect whose commit/rollback calls are observable.
struct FlagEffect {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl agent_contracts::Effect for FlagEffect {
    fn describe(&self) -> String {
        "test effect".into()
    }
    async fn commit(self: Box<Self>) -> agent_contracts::EffectReceipt {
        self.committed.fetch_add(1, Ordering::SeqCst);
        agent_contracts::EffectReceipt::Applied {
            durability: agent_contracts::EffectDurability::Durable,
            evidence: None,
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A dispatcher whose mutating tool stages a `FlagEffect` instead of
/// returning a plain value. `release` lets a test hold the execution open.
struct EffectToolDispatcher {
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
    release: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl ToolDispatcher for EffectToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "stages an effect".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(FlagEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

#[tokio::test]
async fn committed_effect_lands_after_the_generation_fence() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TurnCompleted = envelope.event {
                assert_eq!(committed.load(Ordering::SeqCst), 1);
                assert_eq!(rolled_back.load(Ordering::SeqCst), 0);
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn stale_tool_rolls_back_its_prepared_effect() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Give the tool operation time to start and block inside execute.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();

    // The tool finishes after the cancel: the generation fence has moved, so
    // the actor must roll the prepared effect back instead of committing it.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && rolled_back.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "stale effect must roll back"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "stale effect must never commit"
    );
}

#[tokio::test]
async fn stop_drains_a_cancelled_tool_before_dropping_its_prepared_effect() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Cancellation durably fences the operation but deliberately does not
    // wait for arbitrary tool code. The following Stop must remember that
    // pending cleanup and keep consuming operation completions.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();
    let stop_handle = handle.clone();
    let stop = tokio::spawn(async move { stop_handle.stop().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !stop.is_finished(),
        "Stop must wait for the cancelled tool's explicit cleanup result"
    );

    // The tool returns a PreparedEffect only after cancellation. The actor
    // must route that late completion through the stale rollback path before
    // ending the run, rather than dropping the boxed effect with the channel.
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stop)
        .await
        .expect("Stop must finish once the cancelled tool returns")
        .expect("the actor task must not panic")
        .expect("shutdown cleanup must succeed");
    assert_eq!(rolled_back.load(Ordering::SeqCst), 1);
    assert_eq!(committed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_bounds_a_hanging_tool_scope_close_and_fences_mutation() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(HangingCloseScopeEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: rolled_back.clone(),
            release: Some(release.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let error = tokio::time::timeout(Duration::from_secs(4), handle.cancel_turn())
        .await
        .expect("a replaceable context engine cannot block cancellation forever")
        .expect_err("a timed-out scope close must not acknowledge cancellation as durable");
    assert!(matches!(error, AgentError::RecoveryRequired(_)));
    let mutation_error = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("scope cleanup uncertainty must fence later mutation");
    assert!(matches!(mutation_error, AgentError::RecoveryRequired(_)));

    let mut saw_recovery = false;
    while let Ok(envelope) = events.try_recv() {
        saw_recovery |= matches!(envelope.event, RuntimeEvent::RecoveryRequired);
    }
    assert!(
        saw_recovery,
        "the bounded cleanup failure must be observable"
    );

    // Release the still-running tool so its late PreparedEffect is explicitly
    // rolled back; Stop then has no unresolved operation to abandon.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while rolled_back.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the late prepared effect was not rolled back"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(committed.load(Ordering::SeqCst), 0);
    handle.stop().await.unwrap();
}

// ---------------------------------------------------------------------------
// Capability effects: a capability stages side effects through the same
// unified `EffectRequest` channel as a builtin tool's `PreparedEffect`, and
// the actor commits or rolls them back behind the same generation fence —
// the capability computes, the core executes.
// ---------------------------------------------------------------------------

/// A capability whose one tool stages a `FlagEffect` instead of returning a
/// plain value. `release` lets a test hold the invocation open.
struct StagingCapability {
    manifest: CapabilityManifest,
    committed: Arc<AtomicUsize>,
    rolled_back: Arc<AtomicUsize>,
    release: Option<Arc<tokio::sync::Notify>>,
}

impl StagingCapability {
    fn new(committed: Arc<AtomicUsize>, rolled_back: Arc<AtomicUsize>) -> Self {
        Self {
            manifest: CapabilityManifest {
                id: "staging".into(),
                version: "1.0.0".into(),
                name: "staging capability".into(),
                summary: "stages an effect".into(),
                status: CapabilityStatus::Experimental,
                provides: vec![agent_contracts::CapabilityKind::Tool],
                permissions: vec!["workspace:write".into()],
                requires: Vec::new(),
                tools: vec![ToolSpec {
                    name: "cap.stage".into(),
                    description: "stages an effect".into(),
                    input_schema: json!({"type": "object"}),
                    risk: ToolRisk::WorkspaceWrite,
                    output_budget: None,
                    roles: Vec::new(),
                }],
                lifecycle: CapabilityLifecycle::Lazy,
                transport: CapabilityTransport::Builtin,
                sandbox_profile: Default::default(),
            },
            committed,
            rolled_back,
            release: None,
        }
    }
}

#[async_trait::async_trait]
impl Capability for StagingCapability {
    fn manifest(&self) -> &CapabilityManifest {
        &self.manifest
    }

    async fn invoke(
        &self,
        call: ToolCall,
        _ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(CapabilityOutcome::EffectRequest {
            output: ToolOutput {
                call_id: call.id,
                tool_name: call.name,
                ok: true,
                summary: "staged by capability".into(),
                model_content: "staged by capability".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(FlagEffect {
                committed: self.committed.clone(),
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

/// Calls the capability tool once, then replies plain.
#[derive(Debug, Default)]
struct CapabilityToolModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for CapabilityToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "cap-1".into(),
                    name: "cap.stage".into(),
                    arguments: json!({}),
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

/// Wire a `StagingCapability` into the actor through the capability-aware
/// dispatcher — the composition a real host performs.
async fn spawn_with_staging_capability(capability: StagingCapability) -> RuntimeHandle {
    let registry = Arc::new(CapabilityRegistry::new());
    registry
        .register(Arc::new(capability))
        .expect("capability registers");
    // Loaded tools are the model surface; without a load the actor's
    // round-surface validation would refuse the call before it executes.
    registry
        .load_tool("cap.stage")
        .expect("capability tool loads");
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(TestToolDispatcher),
        registry,
    ));
    spawn_with_approval(
        Arc::new(CapabilityToolModel::default()),
        Arc::new(TestContextEngine),
        dispatcher,
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await
}

#[tokio::test]
async fn capability_effect_requests_commit_behind_the_generation_fence() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_staging_capability(StagingCapability::new(
        committed.clone(),
        rolled_back.clone(),
    ))
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::TurnCompleted = envelope.event {
                assert_eq!(
                    committed.load(Ordering::SeqCst),
                    1,
                    "the capability's staged effect must commit once"
                );
                assert_eq!(
                    rolled_back.load(Ordering::SeqCst),
                    0,
                    "a live capability effect must never roll back"
                );
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn stale_capability_effect_rolls_back() {
    let committed = Arc::new(AtomicUsize::new(0));
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut capability = StagingCapability::new(committed.clone(), rolled_back.clone());
    capability.release = Some(release.clone());
    let handle = spawn_with_staging_capability(capability).await;
    handle.user_message("go".into()).await.unwrap();

    // Give the capability invocation time to start and block inside invoke.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();

    // The capability finishes after the cancel: the generation fence has
    // moved, so the actor must roll its staged effect back — a cancelled
    // capability never mutates the world.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && rolled_back.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        1,
        "a stale capability effect must roll back"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        0,
        "a stale capability effect must never commit"
    );
}

// ---------------------------------------------------------------------------
// Commit receipt classification: `NotApplied` tells the model nothing
// happened; durability failure and `Unknown` fence later mutation because
// the world cannot safely be used as the base for more work.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum CommitResult {
    Durable,
    NotApplied,
    AppliedButDurabilityFailed,
    Unknown,
}

/// An effect that returns the selected structured receipt.
struct ReceiptEffect {
    result: CommitResult,
    rolled_back: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Effect for ReceiptEffect {
    fn describe(&self) -> String {
        "receipt test effect".into()
    }
    async fn commit(self: Box<Self>) -> EffectReceipt {
        match self.result {
            CommitResult::Durable => EffectReceipt::Applied {
                durability: EffectDurability::Durable,
                evidence: Some("durable-test-effect".into()),
            },
            CommitResult::NotApplied => EffectReceipt::NotApplied {
                error: "simulated disk failure".into(),
            },
            CommitResult::AppliedButDurabilityFailed => EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed("simulated journal failure".into()),
                evidence: None,
            },
            CommitResult::Unknown => EffectReceipt::Unknown {
                error: "simulated remote timeout".into(),
            },
        }
    }
    async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
        self.rolled_back.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A dispatcher whose mutating tool stages a `ReceiptEffect`.
struct ReceiptEffectDispatcher {
    result: CommitResult,
    rolled_back: Arc<AtomicUsize>,
    execute_count: Option<Arc<AtomicUsize>>,
}

#[async_trait::async_trait]
impl ToolDispatcher for ReceiptEffectDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "stages a failing effect".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        if let Some(count) = &self.execute_count {
            count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "staged".into(),
                model_content: "staged".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(ReceiptEffect {
                result: self.result,
                rolled_back: self.rolled_back.clone(),
            }),
        })
    }
}

/// Models a mutating dispatcher that staged some internal state but could
/// not prove cleanup before returning. Core must retain the operation for
/// recovery instead of terminalizing it as an ordinary failed value.
struct ExecutionCleanupRecoveryDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for ExecutionCleanupRecoveryDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "fails while settling staged cleanup".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        assert!(
            request.effect_context.is_some(),
            "mutating execution must receive its recovery identity"
        );
        Err(AgentError::RecoveryRequired(
            "simulated prepared cleanup failure".into(),
        ))
    }
}

/// Wait for the tool's finished output (the model-visible result) and the
/// turn completion, returning the handle and observed recovery signal.
async fn run_effect_receipt_turn(
    result: CommitResult,
) -> (RuntimeHandle, ToolOutput, Vec<String>, bool) {
    let rolled_back = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(ReceiptEffectDispatcher {
            result,
            rolled_back: rolled_back.clone(),
            execute_count: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut finished = None;
    let mut warnings = Vec::new();
    let mut recovery_required = false;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. } => finished = Some(output),
                RuntimeEvent::Warning { message } => warnings.push(message),
                RuntimeEvent::RecoveryRequired => recovery_required = true,
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && finished.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(completed, "the turn must complete");
    assert_eq!(
        rolled_back.load(Ordering::SeqCst),
        0,
        "a current-generation commit receipt must not enter the stale rollback path"
    );
    (
        handle,
        finished.expect("the tool must finish"),
        warnings,
        recovery_required,
    )
}

async fn assert_normal_mutation_is_fenced(handle: &RuntimeHandle) {
    let next_message = handle
        .user_message("must wait for recovery".into())
        .await
        .expect_err("an uncertain effect result must fence the next user turn");
    assert!(
        matches!(next_message, AgentError::RecoveryRequired(_)),
        "the next user message must require recovery: {next_message}"
    );

    let next_task_mutation = handle
        .set_focus("must also wait for recovery".into())
        .await
        .expect_err("an uncertain effect result must fence task mutation");
    assert!(
        matches!(next_task_mutation, AgentError::RecoveryRequired(_)),
        "task mutation must require recovery: {next_task_mutation}"
    );
}

#[tokio::test]
async fn execution_cleanup_recovery_is_structured_fenced_and_queryable() {
    let handle = spawn_with_approval(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(ExecutionCleanupRecoveryDispatcher),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut operation_id = None;
    let mut finished = None;
    let mut recovery_required = false;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(envelope)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            match envelope.event {
                RuntimeEvent::OperationAccepted { snapshot }
                    if snapshot.identity.call_id == "call-1" =>
                {
                    operation_id = Some(snapshot.identity.operation_id);
                }
                RuntimeEvent::ToolFinished { output, .. } if output.call_id == "call-1" => {
                    finished = Some(output);
                }
                RuntimeEvent::RecoveryRequired => recovery_required = true,
                _ => {}
            }
        }
        if operation_id.is_some() && finished.is_some() && recovery_required {
            break;
        }
    }

    let output = finished.expect("the cleanup failure must reach ToolFinished");
    assert!(!output.ok);
    assert_eq!(
        output.metadata["commit_state"],
        "execution_cleanup_recovery_required"
    );
    assert_eq!(output.metadata["attempted_paths"], json!([]));
    assert!(output.metadata.get("files").is_none());
    assert!(output.metadata.get("revision").is_none());
    assert!(output.resource_touches().is_empty());
    assert!(!output.model_content.contains("deadbeef"));
    assert!(
        output
            .model_content
            .contains("preparation cleanup could not be confirmed")
    );
    assert!(
        recovery_required,
        "the runtime must publish the recovery fence"
    );

    let operation_id = operation_id.expect("OperationAccepted must expose the recovery identity");
    let queried = handle.query_operation(operation_id).await.unwrap();
    assert!(matches!(
        queried,
        agent_contracts::OperationQueryResult::Found { snapshot }
            if matches!(
                snapshot.state,
                agent_contracts::OperationState::Executing {
                    effect_id: Some(_)
                }
            )
    ));
    assert_normal_mutation_is_fenced(&handle).await;
    assert!(matches!(
        handle.query_operation(operation_id).await.unwrap(),
        agent_contracts::OperationQueryResult::Found { .. }
    ));
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn not_applied_commit_failure_reports_nothing_happened() {
    let (handle, finished, _, recovery_required) =
        run_effect_receipt_turn(CommitResult::NotApplied).await;
    assert!(
        !finished.ok,
        "the failed effect must surface as a failed result"
    );
    assert!(
        finished.model_content.contains("could not be committed"),
        "the model must be told nothing happened, got: {}",
        finished.model_content
    );
    assert!(
        !recovery_required,
        "a definite NotApplied receipt must not poison the runtime"
    );
    handle
        .set_focus("ordinary work may continue".into())
        .await
        .expect("NotApplied leaves a safe base for task mutation");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn applied_but_durability_failure_surfaces_a_recovery_state() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::AppliedButDurabilityFailed).await;
    assert!(
        !finished.ok,
        "the durability failure must surface as a failed result"
    );
    assert!(
        finished.model_content.contains("WAS applied"),
        "the model must be told the change landed but the record failed, got: {}",
        finished.model_content
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains("applied but recovery is required")),
        "the runtime must surface a degraded/recovery warning, got: {warnings:?}"
    );
    assert!(
        recovery_required,
        "the runtime must publish its recovery-required state"
    );
    assert_normal_mutation_is_fenced(&handle).await;
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_effect_state_surfaces_truth_and_fences_later_mutation() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::Unknown).await;
    assert!(!finished.ok, "an unknown applied state is not success");
    assert!(
        finished
            .model_content
            .contains("may or may not have been applied"),
        "the model must receive the uncertain world state: {}",
        finished.model_content
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains("effect applied state unknown")),
        "the runtime must surface an unknown-state warning, got: {warnings:?}"
    );
    assert!(recovery_required, "unknown state must demand recovery");
    assert_normal_mutation_is_fenced(&handle).await;
    handle.stop().await.unwrap();
}

/// An effect broker that accepts reservations and dispatches but always
/// fails the acknowledgement, so a committed effect's typed settlement
/// becomes a debt that must fence later mutation.
struct FailingAckBroker;

#[async_trait::async_trait]
impl agent_core::EffectBroker for FailingAckBroker {
    async fn reserve(&self, reservation: agent_core::EffectReservation) -> AgentResult<String> {
        Ok(format!("broker/{}", reservation.operation_id))
    }

    async fn dispatch(&self, reserved: agent_core::ReservedEffect) -> EffectReceipt {
        reserved.effect.commit().await
    }

    async fn ack(&self, _ack: agent_core::EffectAck) -> AgentResult<()> {
        Err(AgentError::Storage(
            "simulated broker acknowledgement failure".into(),
        ))
    }
}

/// An acknowledgement that cannot be persisted does not hide the applied
/// effect: the typed debt event is published, the runtime demands recovery,
/// and the next mutation is fenced instead of stacking on the debt.
#[tokio::test]
async fn committed_effect_ack_debt_emits_event_and_fences_later_mutation() {
    let committed = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval_and_broker(
        Arc::new(RetryAfterUnknownModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(EffectToolDispatcher {
            committed: committed.clone(),
            rolled_back: Arc::new(AtomicUsize::new(0)),
            release: None,
        }),
        Arc::new(PolicyApprovalGate::permissive()),
        Arc::new(FailingAckBroker),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut ack_debt = None;
    let mut refused = None;
    let mut recovery = false;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::EffectAckDebt { debt } => ack_debt = Some(debt),
                RuntimeEvent::ToolFinished { output, .. }
                    if output.call_id == "must-be-refused" =>
                {
                    refused = Some(output)
                }
                RuntimeEvent::RecoveryRequired => recovery = true,
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused.is_some() && ack_debt.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let debt = ack_debt.expect("the typed ack debt must be published");
    assert_eq!(
        debt.settlement,
        agent_contracts::EffectAckSettlement::Applied {
            durability: EffectDurability::Durable
        },
        "the debt carries the typed settlement of the real receipt"
    );
    assert!(!debt.reservation_id.is_empty());
    assert!(recovery, "an ack debt must demand recovery");
    let refused = refused.expect("the second call must receive a typed refusal");
    assert!(!refused.ok);
    assert_eq!(
        refused.metadata["code"], "runtime.recovery_required",
        "the fence must refuse further mutation: {refused:?}"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        1,
        "the fenced second call must never reach the dispatcher"
    );
    assert!(completed, "the model must still be able to close the turn");
    handle.stop().await.unwrap();
}

#[derive(Debug, Default)]
struct RetryAfterUnknownModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for RetryAfterUnknownModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let tool_calls = match round {
            0 => vec![ToolCall {
                id: "uncertain".into(),
                name: "fs.read".into(),
                arguments: json!({"path": "x"}),
            }],
            1 => vec![ToolCall {
                id: "must-be-refused".into(),
                name: "fs.read".into(),
                arguments: json!({"path": "y"}),
            }],
            _ => Vec::new(),
        };
        Ok(ModelOutput {
            content: if tool_calls.is_empty() {
                "stopped after recovery refusal".into()
            } else {
                String::new()
            },
            tool_calls,
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn recovery_state_refuses_another_tool_in_the_same_turn() {
    let execute_count = Arc::new(AtomicUsize::new(0));
    let handle = spawn_with_approval(
        Arc::new(RetryAfterUnknownModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(ReceiptEffectDispatcher {
            result: CommitResult::Unknown,
            rolled_back: Arc::new(AtomicUsize::new(0)),
            execute_count: Some(execute_count.clone()),
        }),
        Arc::new(PolicyApprovalGate::permissive()),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut refused = None;
    let mut completed = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolFinished { output, .. }
                    if output.call_id == "must-be-refused" =>
                {
                    refused = Some(output)
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if completed && refused.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let refused = refused.expect("the second call must receive a typed refusal");
    assert!(!refused.ok);
    assert_eq!(refused.metadata["executed"], false);
    assert_eq!(refused.metadata["code"], "runtime.recovery_required");
    assert_eq!(
        execute_count.load(Ordering::SeqCst),
        1,
        "only the first uncertain effect may reach the dispatcher"
    );
    assert!(completed, "the model must still be able to close the turn");
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn durable_effect_success_does_not_fence_later_mutation() {
    let (handle, finished, warnings, recovery_required) =
        run_effect_receipt_turn(CommitResult::Durable).await;
    assert!(finished.ok, "a durable commit keeps the successful output");
    assert!(warnings.is_empty(), "durable success needs no warning");
    assert!(
        !recovery_required,
        "durable success must not demand recovery"
    );
    handle
        .set_focus("ordinary work may continue".into())
        .await
        .expect("durable success leaves task mutation enabled");
    handle.stop().await.unwrap();
}

/// A context engine whose `AssistantMessage` ingest always fails: the
/// finalization commit must surface `TurnCommitFailed` + `RecoveryRequired`
/// instead of swallowing the error and clearing the turn silently.
#[derive(Debug)]
struct FailingAssistantIngestEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingAssistantIngestEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::AssistantMessage { .. }) {
            return Err(agent_contracts::AgentError::Context(
                "journal backend unavailable".into(),
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
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

#[tokio::test]
async fn failed_turn_commit_emits_turn_commit_failed_and_recovery_required() {
    let handle = spawn_with(
        Arc::new(PlainModel),
        Arc::new(FailingAssistantIngestEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut commit_failed = None;
    let mut recovery_required = false;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TurnCommitFailed { phase, message } => {
                    commit_failed = Some((phase, message));
                }
                RuntimeEvent::RecoveryRequired => recovery_required = true,
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if commit_failed.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (phase, message) = commit_failed.expect("the failed commit must be journaled");
    assert_eq!(
        phase, "assistant_message_ingest",
        "the failing step must be named"
    );
    assert!(
        message.contains("journal backend unavailable"),
        "the journaled failure must carry the engine error: {message}"
    );
    assert!(
        recovery_required,
        "a failed turn commit must require recovery"
    );
    assert!(
        !turn_completed,
        "a turn whose commit failed must never emit TurnCompleted"
    );

    let next = handle
        .user_message("must not run before recovery".into())
        .await
        .expect_err("a failed mandatory turn commit must fence later mutation");
    assert!(
        matches!(next, agent_contracts::AgentError::RecoveryRequired(_)),
        "the runtime must require a known-good restore after a failed turn commit: {next}"
    );
}

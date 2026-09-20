//! One task crosses a partial effect batch, delayed tool cleanup, a second
//! cancellation, and a stale model reply racing its live continuation.
//! Gates control dependency timing; Runtime and Core own all state changes.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ContextDiagnostics, ContextEngine, ContextGcReport, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, Effect, EffectDurability, EffectReceipt, InputKind, InputLifecycle,
    MaterializedContext, ModelCapabilities, ModelMessage, ModelOutput, ModelRequest, ModelRole,
    ModelTransport, OperationQueryResult, OperationState, OperationTerminal, RuntimeEvent,
    RuntimeEventEnvelope, ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolOperationIdentity, ToolOutcome, ToolOutput, ToolRisk, ToolSpec, TurnCancelAck,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{
    AuthorityRecoveryServices, ContinueReason, ModuleHost, RuntimeInstance, RuntimeServices,
};
use context_baselines::RollingSummaryEngine;
use serde_json::json;
use tokio::sync::{Notify, broadcast};

const DIRECTIVE: &str = "Complete one bounded effect sequence; preserve the committed prefix and never replay cancelled work.";
const COMMITTED: &str = "committed-prefix-A";
const SECOND_COMMITTED: &str = "second-committed-prefix-F";
const CANCELLED: &str = "cancelled-preparation-B";
const QUEUED: &str = "undispatched-sibling-C";
const POISON: &str = "late-model-poison-D";
const RESUMED: &str = "resumed-live-effect-E";

#[derive(Default)]
struct EffectLedger {
    executed: Mutex<Vec<String>>,
    committed: Mutex<Vec<String>>,
    rolled_back: Mutex<Vec<String>>,
    identities: Mutex<BTreeMap<String, ToolOperationIdentity>>,
}

struct RecordedEffect {
    slot: String,
    ledger: Arc<EffectLedger>,
}

#[async_trait::async_trait]
impl Effect for RecordedEffect {
    fn describe(&self) -> String {
        format!("record the bounded effect {}", self.slot)
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        self.ledger
            .committed
            .lock()
            .unwrap()
            .push(self.slot.clone());
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some(self.slot),
        }
    }

    async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
        self.ledger.rolled_back.lock().unwrap().push(self.slot);
        Ok(())
    }
}

struct BatchDispatcher {
    ledger: Arc<EffectLedger>,
    tool_entered: Notify,
    tool_release: Notify,
}

#[async_trait::async_trait]
impl ToolDispatcher for BatchDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "probe.stage".into(),
            description: "stage one bounded observable effect".into(),
            input_schema: json!({"type":"object", "properties":{"slot":{"type":"string"}}, "required":["slot"], "additionalProperties":false}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: Vec::new(),
        }]
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let slot = request.call.arguments["slot"].as_str().unwrap().to_owned();
        self.ledger.executed.lock().unwrap().push(slot.clone());
        self.ledger.identities.lock().unwrap().insert(
            slot.clone(),
            request.effect_context.as_ref().unwrap().identity.clone(),
        );
        if slot == CANCELLED {
            self.tool_entered.notify_one();
            // Deliberately returns a prepared effect after cancellation. This
            // must use Core rollback, even though the original batch is gone.
            self.tool_release.notified().await;
        }
        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: slot.clone(),
                model_content: slot.clone(),
                artifact_ref: None,
                metadata: json!({}),
            },
            effect: Box::new(RecordedEffect {
                slot,
                ledger: self.ledger.clone(),
            }),
        })
    }
}

#[derive(Default)]
struct SequenceModel {
    rounds: AtomicUsize,
    requests: Mutex<Vec<Vec<ModelMessage>>>,
    old_entered: Notify,
    old_release: Notify,
    live_entered: Notify,
    live_release: Notify,
}

fn call(slot: &str) -> ToolCall {
    ToolCall {
        id: slot.into(),
        name: "probe.stage".into(),
        arguments: json!({"slot":slot}),
    }
}

#[async_trait::async_trait]
impl ModelTransport for SequenceModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(request.messages);
        let tool_calls = match self.rounds.fetch_add(1, Ordering::SeqCst) {
            0 => vec![call(COMMITTED), call(CANCELLED), call(QUEUED)],
            1 => {
                self.old_entered.notify_one();
                self.old_release.notified().await;
                vec![call(POISON)]
            }
            2 => {
                self.live_entered.notify_one();
                self.live_release.notified().await;
                vec![call(RESUMED)]
            }
            3 => Vec::new(),
            round => panic!("unexpected replay or extra model request {round}"),
        };
        Ok(ModelOutput {
            content: if tool_calls.is_empty() {
                "live continuation finished".into()
            } else {
                String::new()
            },
            tool_calls,
            usage: Default::default(),
        })
    }
}

async fn gate(entered: &Notify) {
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("the expected dependency boundary must be reached");
}

async fn collect_until(
    events: &mut broadcast::Receiver<RuntimeEventEnvelope>,
    trace: &mut Vec<RuntimeEvent>,
    reached: impl Fn(&RuntimeEvent) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("runtime event stream stays complete")
                .event;
            let done = reached(&event);
            trace.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .expect("runtime must reach the expected event boundary");
}

async fn instance(
    root: &Path,
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
    dispatcher: Arc<BatchDispatcher>,
) -> RuntimeInstance {
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
        dispatcher,
        Arc::new(PolicyApprovalGate::permissive()),
        Some(journal),
        AuthorityRecoveryServices::new(
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
    let runtime = RuntimeInstance::spawn(host, services);
    runtime.start().await.unwrap();
    runtime
}

#[tokio::test]
async fn partial_batch_cleanup_and_late_model_keep_one_task_and_exact_effects() {
    combined_cancellation(false, false).await;
}

#[tokio::test]
async fn operation_cancel_preserves_the_committed_prefix_through_cold_restore() {
    combined_cancellation(true, false).await;
}

#[tokio::test]
async fn dynamic_context_keeps_cancelled_batch_prefix_retrievable_after_scope_close_and_cold_restore()
 {
    combined_cancellation(false, true).await;
}

fn context_engine(root: &Path, dynamic: bool) -> Arc<dyn ContextEngine> {
    if dynamic {
        Arc::new(context_simple::SimpleContextEngine::new(
            context_simple::SimpleContextConfig {
                context_store_dir: Some(root.join("context-store")),
                ..Default::default()
            },
        ))
    } else {
        Arc::new(RollingSummaryEngine::new())
    }
}

fn retained_directive_count(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                usize::from(key == "content" && value.as_str() == Some(DIRECTIVE))
                    + retained_directive_count(value)
            })
            .sum(),
        serde_json::Value::Array(values) => values.iter().map(retained_directive_count).sum(),
        _ => 0,
    }
}

fn matching_observations(value: &serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Object(fields)
            if fields
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains(COMMITTED)) =>
        {
            vec![value.clone()]
        }
        serde_json::Value::Object(fields) => {
            fields.values().flat_map(matching_observations).collect()
        }
        serde_json::Value::Array(values) => values.iter().flat_map(matching_observations).collect(),
        _ => Vec::new(),
    }
}

async fn assert_dynamic_observation(
    context: &dyn ContextEngine,
    expected: &agent_contracts::ContextItem,
    operation: &ToolOperationIdentity,
) {
    assert_eq!(expected.task_id, operation.task_id);
    assert_eq!(expected.scope_id, operation.scope_id);
    let matches = context
        .search_external(agent_contracts::ContextSearchQuery {
            kind: Some(agent_contracts::ContextKind::ToolObservation),
            task_id: operation.task_id,
            limit: 16,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        matches.iter().any(|item| item.item_id == expected.id
            && item.task_id == operation.task_id
            && item.scope_id == operation.scope_id),
        "the original task-scoped observation remains discoverable"
    );
    let fetched = context
        .fetch_external(expected.id)
        .await
        .unwrap()
        .expect("accepted observation remains fetchable through the ContextEngine contract");
    assert_eq!(fetched.id, expected.id);
    assert_eq!(fetched.task_id, expected.task_id);
    assert_eq!(fetched.scope_id, expected.scope_id);
    assert_eq!(fetched.content, COMMITTED);
    assert_eq!(fetched.source.as_deref(), Some("tool:probe.stage"));
    assert!(fetched.semantic.is_live());
    // Explicit retrieval may require a known item without retuning the
    // engine's ordinary scoring or permanently pinning an opaque result.
    let materialized = context
        .materialize(ContextQuery {
            current_input: DIRECTIVE.into(),
            budget_tokens: 4096,
            hints: agent_contracts::ContextHints {
                anchor_roots: vec![agent_contracts::AnchorRootClaim {
                    item_ref: expected.id.to_string(),
                    strength: agent_contracts::AnchorRootStrength::PromptRequired,
                    source_field_id: "evidence_refs".into(),
                    anchor_revision: 1,
                    reason: agent_contracts::RootReason::TaskAnchor,
                }],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    assert!(materialized.required_misses.is_empty());
    assert!(
        materialized
            .items
            .iter()
            .any(|item| item.item_id == expected.id && item.content == COMMITTED)
    );
}

async fn combined_cancellation(cancel_by_identity: bool, dynamic: bool) {
    let ledger = Arc::new(EffectLedger::default());
    let dispatcher = Arc::new(BatchDispatcher {
        ledger: ledger.clone(),
        tool_entered: Notify::new(),
        tool_release: Notify::new(),
    });
    let model = Arc::new(SequenceModel::default());
    let directory = tempfile::tempdir().unwrap();
    let context = context_engine(directory.path(), dynamic);
    let runtime = instance(
        directory.path(),
        model.clone(),
        context.clone(),
        dispatcher.clone(),
    )
    .await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    let mut trace = Vec::new();
    let submission = handle
        .start_work(DIRECTIVE.into(), "combined-cancel".into())
        .await
        .unwrap();

    gate(&dispatcher.tool_entered).await;
    assert_eq!(*ledger.executed.lock().unwrap(), [COMMITTED, CANCELLED]);
    assert_eq!(*ledger.committed.lock().unwrap(), [COMMITTED]);
    let first_turn = if cancel_by_identity {
        let identity = ledger.identities.lock().unwrap()[CANCELLED].clone();
        let OperationQueryResult::Found { snapshot } =
            handle.cancel_operation(identity.clone()).await.unwrap()
        else {
            panic!("the cancelled operation must retain its Core terminal");
        };
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                terminal: OperationTerminal::CancelledBeforeCommit,
                ..
            }
        ));
        identity.turn_id
    } else {
        match handle.cancel_turn().await.unwrap() {
            TurnCancelAck::Cancelled {
                turn_id, task_id, ..
            } => {
                assert_eq!(task_id, Some(submission.task_id));
                turn_id
            }
            TurnCancelAck::NoActiveTurn => panic!("the partial batch is still running"),
        }
    };
    assert_eq!(
        handle
            .status_snapshot()
            .await
            .unwrap()
            .continue_readiness
            .reason,
        ContinueReason::CleanupInFlight
    );
    assert!(matches!(
        handle.continue_active_task().await,
        Err(AgentError::InvalidRequest(_))
    ));
    assert_eq!(
        model.rounds.load(Ordering::SeqCst),
        1,
        "cleanup refusal cannot start a continuation"
    );

    dispatcher.tool_release.notify_one();
    collect_until(&mut events, &mut trace, |event| {
        matches!(event, RuntimeEvent::Warning { message } if message.contains("stale tool result dropped"))
    }).await;
    assert_eq!(*ledger.rolled_back.lock().unwrap(), [CANCELLED]);
    assert_eq!(*ledger.committed.lock().unwrap(), [COMMITTED]);
    let before = runtime.checkpoint().await.unwrap();
    let task_before = before
        .tasks
        .tasks
        .iter()
        .find(|task| task.id == submission.task_id)
        .unwrap();
    let directive_before = task_before
        .current_directive
        .as_ref()
        .unwrap()
        .input
        .clone();
    let directive_revision = task_before.resume.directive_revision;
    let dynamic_observation: Option<agent_contracts::ContextItem> = if dynamic {
        let observations = matching_observations(&before.context);
        assert_eq!(
            observations.len(),
            1,
            "the settled prefix is retained exactly once"
        );
        Some(serde_json::from_value(observations[0].clone()).unwrap())
    } else {
        None
    };

    assert_eq!(
        handle.continue_active_task().await.unwrap(),
        submission.task_id
    );
    gate(&model.old_entered).await;
    let second_turn = match handle.cancel_turn().await.unwrap() {
        TurnCancelAck::Cancelled {
            turn_id, task_id, ..
        } => {
            assert_eq!(task_id, Some(submission.task_id));
            turn_id
        }
        TurnCancelAck::NoActiveTurn => panic!("the model continuation is still running"),
    };
    assert_ne!(first_turn, second_turn);
    assert_eq!(
        handle.cancel_turn().await.unwrap(),
        TurnCancelAck::NoActiveTurn
    );

    assert_eq!(
        handle.continue_active_task().await.unwrap(),
        submission.task_id
    );
    gate(&model.live_entered).await;
    model.old_release.notify_one();
    collect_until(&mut events, &mut trace, |event| {
        matches!(event, RuntimeEvent::Warning { message } if message.contains("stale model result dropped"))
    }).await;
    assert_eq!(
        handle
            .status_snapshot()
            .await
            .unwrap()
            .continue_readiness
            .reason,
        ContinueReason::TurnRunning,
        "a stale model reply cannot clear the current operation"
    );
    assert_eq!(
        *ledger.executed.lock().unwrap(),
        [COMMITTED, CANCELLED],
        "neither the old batch tail nor stale model tool calls may execute"
    );

    model.live_release.notify_one();
    collect_until(&mut events, &mut trace, |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await;
    assert_eq!(
        *ledger.executed.lock().unwrap(),
        [COMMITTED, CANCELLED, RESUMED]
    );
    assert_eq!(*ledger.committed.lock().unwrap(), [COMMITTED, RESUMED]);
    assert_eq!(*ledger.rolled_back.lock().unwrap(), [CANCELLED]);
    let identities = ledger.identities.lock().unwrap().clone();
    for (slot, identity) in identities {
        assert_eq!(identity.task_id, Some(submission.task_id));
        let OperationQueryResult::Found { snapshot } =
            handle.query_operation(identity.operation_id).await.unwrap()
        else {
            panic!("Core must retain the operation terminal for {slot}");
        };
        assert_eq!(snapshot.identity, identity);
        match snapshot.state {
            OperationState::Terminal {
                terminal: OperationTerminal::CancelledBeforeCommit,
                ..
            } => assert_eq!(slot, CANCELLED),
            OperationState::Terminal {
                terminal:
                    OperationTerminal::Applied {
                        durability: EffectDurability::Durable,
                        evidence,
                    },
                ..
            } => {
                assert!(slot == COMMITTED || slot == RESUMED);
                assert_eq!(evidence.as_deref(), Some(slot.as_str()));
            }
            state => panic!("unexpected Core terminal for {slot}: {state:?}"),
        }
    }

    let after = runtime.checkpoint().await.unwrap();
    assert_eq!(after.current_task_id, Some(submission.task_id));
    assert_eq!(after.tasks.tasks.len(), 1);
    let task_after = &after.tasks.tasks[0];
    assert_eq!(task_after.resume.directive_revision, directive_revision);
    assert_eq!(
        task_after.current_directive.as_ref().unwrap().input,
        directive_before
    );
    let context_data = context.checkpoint().await.unwrap();
    let context_text = serde_json::to_string(&context_data).unwrap();
    assert!(context_text.contains(RESUMED));
    for forbidden in [CANCELLED, QUEUED, POISON] {
        assert!(
            !context_text.contains(forbidden),
            "cancelled work must not become context evidence: {forbidden}"
        );
    }
    assert_eq!(
        retained_directive_count(&context_data),
        1,
        "continuations must not re-ingest the original instruction"
    );
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for messages in requests.iter() {
            assert!(
                messages
                    .iter()
                    .any(|message| message.role == ModelRole::User && message.content == DIRECTIVE)
            );
            assert!(
                !messages
                    .iter()
                    .any(|message| message.content.contains(POISON))
            );
        }
    }
    for cancelled_turn in [first_turn, second_turn] {
        assert_eq!(trace.iter().filter(|event| matches!(event, RuntimeEvent::TurnCancelled { turn_id, .. } if *turn_id == cancelled_turn)).count(), 1, "each cancelled turn must publish exactly one terminal");
    }
    assert_eq!(
        trace
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::TurnCompleted))
            .count(),
        1
    );
    assert!(!trace.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnFailed { .. }
            | RuntimeEvent::TurnCommitFailed { .. }
            | RuntimeEvent::RecoveryRequired
    )));
    assert_eq!(trace.iter().filter(|event| matches!(event, RuntimeEvent::UserMessageAccepted { input } if input.kind == InputKind::TaskContinuation && input.lifecycle == InputLifecycle::Applied && input.causal_parent == directive_before.input_id)).count(), 2);
    let committed_in_continuations: Vec<bool> = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .skip(1)
        .map(|messages| {
            messages
                .iter()
                .any(|message| message.content.contains(COMMITTED))
        })
        .collect();
    let checkpoint_store =
        agent_runtime::CheckpointStore::new(directory.path().join("explicit-checkpoints"));
    let stored = checkpoint_store
        .write_atomic(&serde_json::to_vec(&before).unwrap())
        .await
        .unwrap();
    let decoded = agent_runtime::decode_checkpoint_file(
        &directory
            .path()
            .join("explicit-checkpoints")
            .join(stored.artifact),
    )
    .unwrap();
    if let Some(observation) = &dynamic_observation {
        let operation = ledger.identities.lock().unwrap()[COMMITTED].clone();
        assert_dynamic_observation(context.as_ref(), observation, &operation).await;
    } else {
        assert_eq!(
            committed_in_continuations,
            [true, true, true],
            "Rolling's retained history must expose the durable prefix to every continuation"
        );
    }
    runtime.shutdown().await.unwrap();
    assert!(
        serde_json::to_string(&before).unwrap().contains(COMMITTED),
        "the cancelled batch's committed prefix must survive the checkpoint boundary"
    );
    let cold_model = Arc::new(SequenceModel {
        rounds: AtomicUsize::new(3),
        ..Default::default()
    });
    let cold_context = context_engine(directory.path(), dynamic);
    let cold = instance(
        directory.path(),
        cold_model.clone(),
        cold_context.clone(),
        dispatcher,
    )
    .await;
    cold.restore(decoded).await.unwrap();
    let mut cold_events = cold.handle().subscribe();
    assert_eq!(
        cold.handle().continue_active_task().await.unwrap(),
        submission.task_id
    );
    collect_until(&mut cold_events, &mut Vec::new(), |event| {
        matches!(event, RuntimeEvent::TurnCompleted)
    })
    .await;
    {
        let cold_requests = cold_model.requests.lock().unwrap();
        assert_eq!(cold_requests.len(), 1);
        if !dynamic {
            assert!(
                cold_requests[0]
                    .iter()
                    .any(|message| message.content.contains(COMMITTED))
            );
        }
        assert!(
            cold_requests[0]
                .iter()
                .any(|message| message.role == ModelRole::User && message.content == DIRECTIVE)
        );
    }
    if let Some(observation) = &dynamic_observation {
        let operation = ledger.identities.lock().unwrap()[COMMITTED].clone();
        assert_dynamic_observation(cold_context.as_ref(), observation, &operation).await;
    }
    assert_eq!(
        *ledger.executed.lock().unwrap(),
        [COMMITTED, CANCELLED, RESUMED],
        "cold restore may not replay the accepted prefix or abandoned siblings"
    );
    cold.shutdown().await.unwrap();
}

enum GateBoundary {
    Observation,
    SecondObservationFailure,
    FinalGc,
}

struct GatedContext {
    inner: RollingSummaryEngine,
    boundary: GateBoundary,
    entered: Notify,
    dropped: Arc<AtomicBool>,
    observation_attempts: AtomicUsize,
}

struct FutureDrop(Arc<AtomicBool>);
impl Drop for FutureDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl GatedContext {
    fn new(boundary: GateBoundary) -> Self {
        Self {
            inner: RollingSummaryEngine::new(),
            boundary,
            entered: Notify::new(),
            dropped: Arc::new(AtomicBool::new(false)),
            observation_attempts: AtomicUsize::new(0),
        }
    }
    async fn hang(&self) {
        let _drop = FutureDrop(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}

#[async_trait::async_trait]
impl ContextEngine for GatedContext {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if matches!(ingress, ContextIngress::ToolObservation { .. }) {
            let attempt = self.observation_attempts.fetch_add(1, Ordering::SeqCst);
            match self.boundary {
                GateBoundary::Observation => self.hang().await,
                GateBoundary::SecondObservationFailure if attempt == 1 => {
                    return Err(AgentError::Context(
                        "second accepted observation could not be persisted".into(),
                    ));
                }
                _ => {}
            }
        }
        self.inner.ingest(ingress).await
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.inner.maintain(trigger).await
    }
    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.inner.materialize(query).await
    }
    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        self.inner.open_scope(kind, parent).await
    }
    async fn close_scope(&self, scope: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.inner.close_scope(scope).await
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        self.inner.diagnostics().await
    }
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        self.inner.inspect(limit).await
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        self.inner.checkpoint().await
    }
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
        self.inner.restore(data).await
    }
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        if matches!(self.boundary, GateBoundary::FinalGc) {
            self.hang().await;
        }
        self.inner.gc().await
    }
}

#[derive(Default)]
struct TwoAcceptedResultsModel {
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for TwoAcceptedResultsModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        assert_eq!(
            self.rounds.fetch_add(1, Ordering::SeqCst),
            0,
            "a failed preservation must fence every later model request"
        );
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![call(COMMITTED), call(SECOND_COMMITTED), call(CANCELLED)],
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn partial_cancelled_prefix_ingest_failure_fences_without_retrying_accepted_results() {
    partial_preservation_failure(false).await;
}

#[tokio::test]
async fn operation_cancel_partial_prefix_ingest_failure_fences_without_retrying_accepted_results() {
    partial_preservation_failure(true).await;
}

async fn partial_preservation_failure(cancel_by_identity: bool) {
    let directory = tempfile::tempdir().unwrap();
    let ledger = Arc::new(EffectLedger::default());
    let dispatcher = Arc::new(BatchDispatcher {
        ledger: ledger.clone(),
        tool_entered: Notify::new(),
        tool_release: Notify::new(),
    });
    let context = Arc::new(GatedContext::new(GateBoundary::SecondObservationFailure));
    let model = Arc::new(TwoAcceptedResultsModel::default());
    let runtime = instance(
        directory.path(),
        model.clone(),
        context.clone(),
        dispatcher.clone(),
    )
    .await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle
        .start_work(DIRECTIVE.into(), "partial-ingest-failure".into())
        .await
        .unwrap();
    gate(&dispatcher.tool_entered).await;
    assert_eq!(
        *ledger.committed.lock().unwrap(),
        [COMMITTED, SECOND_COMMITTED]
    );
    let identities = ledger.identities.lock().unwrap().clone();
    for slot in [COMMITTED, SECOND_COMMITTED] {
        let identity = &identities[slot];
        let OperationQueryResult::Found { snapshot } =
            handle.query_operation(identity.operation_id).await.unwrap()
        else {
            panic!("Core must already have accepted {slot} before cancellation");
        };
        assert_eq!(snapshot.identity, *identity);
        assert!(matches!(
            snapshot.state,
            OperationState::Terminal {
                terminal: OperationTerminal::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(ref evidence),
                },
                ..
            } if evidence == slot
        ));
    }

    let error = if cancel_by_identity {
        handle
            .cancel_operation(identities[CANCELLED].clone())
            .await
            .unwrap_err()
    } else {
        handle.cancel_turn().await.unwrap_err()
    };
    assert!(matches!(
        error,
        AgentError::RecoveryRequired(ref reason)
            if reason.contains("second accepted observation could not be persisted")
    ));
    assert_eq!(context.observation_attempts.load(Ordering::SeqCst), 2);
    let partially_persisted = context.checkpoint().await.unwrap();
    let observations = matching_observations(&partially_persisted);
    assert_eq!(observations.len(), 1, "the first observation was persisted");
    let persisted_text = serde_json::to_string(&partially_persisted).unwrap();
    assert!(!persisted_text.contains(SECOND_COMMITTED));
    assert!(!persisted_text.contains(CANCELLED));

    assert_eq!(
        handle.cancel_turn().await.unwrap(),
        TurnCancelAck::NoActiveTurn
    );
    assert!(matches!(
        handle.cancel_operation(identities[CANCELLED].clone()).await,
        Err(AgentError::RecoveryRequired(_))
    ));
    assert!(matches!(
        handle.continue_active_task().await,
        Err(AgentError::RecoveryRequired(_))
    ));
    dispatcher.tool_release.notify_one();
    let mut trace = Vec::new();
    collect_until(&mut events, &mut trace, |event| matches!(event, RuntimeEvent::Warning { message } if message.contains("stale tool result dropped"))).await;
    assert_eq!(*ledger.rolled_back.lock().unwrap(), [CANCELLED]);
    assert_eq!(
        *ledger.executed.lock().unwrap(),
        [COMMITTED, SECOND_COMMITTED, CANCELLED]
    );
    assert_eq!(
        *ledger.committed.lock().unwrap(),
        [COMMITTED, SECOND_COMMITTED]
    );
    assert_eq!(model.rounds.load(Ordering::SeqCst), 1);
    assert_eq!(
        context.observation_attempts.load(Ordering::SeqCst),
        2,
        "repeat cancellation and continuation must not retry a partial ingest"
    );
    assert_eq!(
        matching_observations(&context.checkpoint().await.unwrap()),
        observations,
        "the original observation remains exactly once, with the same identity"
    );
    runtime.shutdown().await.unwrap();
    collect_until(&mut events, &mut trace, |event| {
        matches!(event, RuntimeEvent::RunCompleted)
    })
    .await;
    assert!(!trace.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnCancelled { .. } | RuntimeEvent::TurnCompleted
    )));
    assert_eq!(
        trace.iter().filter(|event| matches!(event, RuntimeEvent::TurnCommitFailed { phase, .. } if phase == "tool_observation_ingest")).count(),
        1
    );
    assert!(
        trace
            .iter()
            .any(|event| matches!(event, RuntimeEvent::RecoveryRequired))
    );
}

#[tokio::test]
async fn cancelled_prefix_ingest_timeout_joins_and_fences_without_success_ack() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = Arc::new(EffectLedger::default());
    let dispatcher = Arc::new(BatchDispatcher {
        ledger: ledger.clone(),
        tool_entered: Notify::new(),
        tool_release: Notify::new(),
    });
    let context = Arc::new(GatedContext::new(GateBoundary::Observation));
    let runtime = instance(
        directory.path(),
        Arc::new(SequenceModel::default()),
        context.clone(),
        dispatcher.clone(),
    )
    .await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle
        .start_work(DIRECTIVE.into(), "hanging-ingest".into())
        .await
        .unwrap();
    gate(&dispatcher.tool_entered).await;
    let cancel = tokio::spawn({
        let handle = handle.clone();
        async move { handle.cancel_turn().await }
    });
    gate(&context.entered).await;
    let error = tokio::time::timeout(Duration::from_secs(10), cancel)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, AgentError::RecoveryRequired(_)));
    assert!(
        context.dropped.load(Ordering::SeqCst),
        "failed preservation must abort and join before returning"
    );
    assert!(matches!(
        handle.continue_active_task().await,
        Err(AgentError::RecoveryRequired(_))
    ));
    dispatcher.tool_release.notify_one();
    let mut trace = Vec::new();
    collect_until(&mut events, &mut trace, |event| matches!(event, RuntimeEvent::Warning { message } if message.contains("stale tool result dropped"))).await;
    assert_eq!(*ledger.committed.lock().unwrap(), [COMMITTED]);
    assert_eq!(*ledger.rolled_back.lock().unwrap(), [CANCELLED]);
    assert!(!trace.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnCancelled { .. } | RuntimeEvent::TurnCompleted
    )));
    assert!(trace.iter().any(|event| matches!(event, RuntimeEvent::TurnCommitFailed { phase, .. } if phase == "tool_observation_ingest")));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_final_gc_does_not_duplicate_already_persisted_effect_observations() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = Arc::new(EffectLedger::default());
    let dispatcher = Arc::new(BatchDispatcher {
        ledger: ledger.clone(),
        tool_entered: Notify::new(),
        tool_release: Notify::new(),
    });
    let context = Arc::new(GatedContext::new(GateBoundary::FinalGc));
    let model = Arc::new(SequenceModel {
        rounds: AtomicUsize::new(2),
        ..Default::default()
    });
    model.live_release.notify_one();
    let runtime = instance(directory.path(), model, context.clone(), dispatcher).await;
    let handle = runtime.handle();
    let mut events = handle.subscribe();
    handle
        .start_work(DIRECTIVE.into(), "final-gc".into())
        .await
        .unwrap();
    gate(&context.entered).await;
    assert!(matches!(
        handle.cancel_turn().await.unwrap(),
        TurnCancelAck::Cancelled { .. }
    ));
    assert!(context.dropped.load(Ordering::SeqCst));
    let data = context.checkpoint().await.unwrap();
    assert_eq!(
        data["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["content"]
                .as_str()
                .is_some_and(|content| content.contains(RESUMED)))
            .count(),
        1,
        "normal finalization already persisted the observation before GC"
    );
    assert_eq!(*ledger.committed.lock().unwrap(), [RESUMED]);
    let mut trace = Vec::new();
    collect_until(&mut events, &mut trace, |event| {
        matches!(event, RuntimeEvent::TurnCancelled { .. })
    })
    .await;
    assert!(!trace.iter().any(|event| matches!(
        event,
        RuntimeEvent::TurnCompleted | RuntimeEvent::RecoveryRequired
    )));
    runtime.shutdown().await.unwrap();
}

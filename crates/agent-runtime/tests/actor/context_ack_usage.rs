//! QB (review Q2): a model round's reported usage settles through the shared
//! account independently of the business-acceptance branch. A ContextEngine
//! consumption-ACK failure must still refuse the tool dispatch and refuse the
//! round's result, but it must not delete the cost the provider already
//! reported; and a failed account write must not be claimed as a clean round.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ContextConsumptionAck, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextQuery, ContextStateTransition, EventJournal, MaterializedContext, ModelCapabilities,
    ModelOutput, ModelRequest, ModelTransport, ModelUsage, RuntimeEvent, RuntimeEventEnvelope,
    ScopeId, ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolRisk, ToolSemanticRole, ToolSpec,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};

/// A context engine that behaves like the harness stub but can refuse the
/// final consumption ACK: the injected internal-commit failure of review Q2.
#[derive(Debug, Default)]
struct AckFailingContextEngine {
    fail_acks: AtomicBool,
}

#[async_trait::async_trait]
impl ContextEngine for AckFailingContextEngine {
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
    async fn acknowledge_consumption(&self, _ack: ContextConsumptionAck) -> AgentResult<()> {
        if self.fail_acks.load(Ordering::SeqCst) {
            return Err(AgentError::Context(
                "simulated consumption ack failure".into(),
            ));
        }
        Ok(())
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

/// One legitimate tool call plus explicit provider usage.
#[derive(Debug, Default)]
struct ToolCallWithUsageModel;

#[async_trait::async_trait]
impl ModelTransport for ToolCallWithUsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            ..ModelCapabilities::default()
        }
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-ack-1".into(),
                name: "fs.read".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
            }],
            usage: ModelUsage {
                input_tokens: Some(120),
                output_tokens: Some(30),
                cached_input_tokens: Some(60),
                attempts: 1,
                retries: 0,
                ..Default::default()
            },
        })
    }
}

/// A plain final answer with explicit usage: the round completes normally.
#[derive(Debug, Default)]
struct FinalAnswerWithUsageModel;

#[async_trait::async_trait]
impl ModelTransport for FinalAnswerWithUsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: ModelUsage {
                input_tokens: Some(120),
                output_tokens: Some(30),
                cached_input_tokens: Some(60),
                attempts: 1,
                retries: 0,
                ..Default::default()
            },
        })
    }
}

/// A dispatcher that surfaces the real fs.read spec and counts executions, so
/// "the refused round never dispatched its tool call" is directly observable.
#[derive(Debug, Default)]
struct CountingReadDispatcher {
    executions: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolDispatcher for CountingReadDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a workspace file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "read".into(),
            model_content: "file body".into(),
            artifact_ref: None,
            metadata: serde_json::json!({}),
        }))
    }
}

fn kernel_with_dispatcher(
    model: Arc<dyn ModelTransport>,
    context: Arc<dyn ContextEngine>,
    dispatcher: Arc<dyn ToolDispatcher>,
) -> Arc<RuntimeServices> {
    Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context,
        model,
        dispatcher,
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ))
}

/// The event journal that fails exactly the account write: the ModelUsed
/// append is the settlement this slice must not claim as succeeded.
#[derive(Debug, Default)]
struct FailModelUsedJournal;

#[async_trait::async_trait]
impl EventJournal for FailModelUsedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::ModelUsed { .. }) {
            return Err(AgentError::Storage(
                "simulated model-usage journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct UsageRows {
    rows: Vec<agent_contracts::ModelUsage>,
    identities: Vec<agent_contracts::UsageIdentity>,
}

fn scan_usage_rows(envelopes: &[RuntimeEventEnvelope]) -> UsageRows {
    let mut rows = UsageRows::default();
    for envelope in envelopes {
        if let RuntimeEvent::ModelUsed {
            usage: Some(usage),
            usage_identity,
            ..
        } = &envelope.event
        {
            rows.rows.push(usage.clone());
            rows.identities.push(*usage_identity);
        }
    }
    rows
}

/// QB (Q2) red phase: the provider reported real counters, the consumption
/// ACK fails, and the account must still receive exactly that usage once —
/// while the tool call stays undispatched and the failure stays honest.
#[tokio::test]
async fn ack_failure_keeps_the_reported_model_usage_in_the_account() {
    let engine = Arc::new(AckFailingContextEngine::default());
    engine.fail_acks.store(true, Ordering::SeqCst);
    let dispatcher = Arc::new(CountingReadDispatcher::default());
    let (handle, _task): (RuntimeHandle, tokio::task::JoinHandle<()>) =
        spawn_runtime(kernel_with_dispatcher(
            Arc::new(ToolCallWithUsageModel),
            engine.clone(),
            dispatcher.clone(),
        ));
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle.user_message("use the tool".into()).await.unwrap();

    // The round is refused: the internal commit failure surfaces as an error,
    // and the tool call behind it is never dispatched.
    let mut seen: Vec<RuntimeEventEnvelope> = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(envelope) => {
                    if let RuntimeEvent::Error { message } = &envelope.event {
                        assert!(
                            message.contains("failed to commit model context consumption"),
                            "the ack failure must surface truthfully: {message}"
                        );
                        break;
                    }
                    seen.push(envelope);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed before the ack failure surfaced")
                }
            }
        }
    })
    .await
    .expect("the ack failure did not surface within the test deadline");

    // Grace window for the (already-emitted) settlement row to be observed.
    tokio::time::timeout(Duration::from_millis(300), async {
        while let Ok(envelope) = events.try_recv() {
            seen.push(envelope);
        }
    })
    .await
    .unwrap_or(());

    let rows = scan_usage_rows(&seen);
    assert_eq!(
        rows.rows.len(),
        1,
        "the reported usage must be accounted exactly once, got {:?}",
        rows.rows
    );
    let usage = &rows.rows[0];
    assert_eq!(usage.input_tokens, Some(120));
    assert_eq!(usage.output_tokens, Some(30));
    assert_eq!(usage.cached_input_tokens, Some(60));
    assert_eq!(rows.identities[0], agent_contracts::UsageIdentity::Observed);
    assert_eq!(
        dispatcher.executions.load(Ordering::SeqCst),
        0,
        "a refused round must not dispatch its tool call"
    );
    assert!(
        !seen
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TurnCompleted)),
        "the refused round must not be reported as completed"
    );
    handle.stop().await.unwrap();
}

/// The healthy path keeps its original semantics: one round, one usage row,
/// and the turn completes.
#[tokio::test]
async fn healthy_ack_still_settles_the_usage_exactly_once() {
    let engine = Arc::new(AckFailingContextEngine::default());
    let (handle, _task) = spawn_runtime(kernel_with_dispatcher(
        Arc::new(FinalAnswerWithUsageModel),
        engine.clone(),
        Arc::new(CountingReadDispatcher::default()),
    ));
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle.user_message("finish it".into()).await.unwrap();

    // Collect every event up to and including the completion: the settlement
    // row precedes TurnCompleted, so the wait loop must keep what it sees.
    let mut seen: Vec<RuntimeEventEnvelope> = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(envelope) => {
                    let completed = matches!(envelope.event, RuntimeEvent::TurnCompleted);
                    seen.push(envelope);
                    if completed {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed before TurnCompleted")
                }
            }
        }
    })
    .await
    .expect("the turn did not commit within the test deadline");

    let rows = scan_usage_rows(&seen);
    assert_eq!(
        rows.rows.len(),
        1,
        "one round settles once: {:?}",
        rows.rows
    );
    assert_eq!(rows.rows[0].input_tokens, Some(120));
    assert_eq!(rows.rows[0].output_tokens, Some(30));
    handle.stop().await.unwrap();
}

/// A failed account write is not a settled booking: the runtime keeps the
/// recoverable settlement obligation (fences, reports the failed phase) and
/// never claims the round completed cleanly.
#[tokio::test]
async fn model_usage_journal_failure_does_not_claim_a_clean_round() {
    let journal = Arc::new(FailModelUsedJournal);
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(AckFailingContextEngine::default()),
        Arc::new(FinalAnswerWithUsageModel),
        Arc::new(CountingReadDispatcher::default()),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    handle.start().await.unwrap();
    let mut events = handle.subscribe();

    handle.user_message("finish it".into()).await.unwrap();

    // Either the failed settlement phase is reported (fixed behavior) or the
    // turn is (wrongly) completed as if the account write had succeeded.
    let failed_phase = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(envelope) => match &envelope.event {
                    RuntimeEvent::TurnCommitFailed { phase, message } => {
                        assert_eq!(phase, "model_usage_settled_event");
                        assert!(
                            message.contains("simulated model-usage journal failure"),
                            "the account-write failure must be named: {message}"
                        );
                        break true;
                    }
                    RuntimeEvent::TurnCompleted => break false,
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event stream closed before the round settled")
                }
            }
        }
    })
    .await
    .expect("the round did not settle within the test deadline");
    assert!(
        failed_phase,
        "a failed account write must be reported, not claimed as a clean round"
    );
    // The obligation stays recoverable: the runtime is fenced until the
    // account write is reconciled, so further mutation is refused.
    let error = handle.user_message("again".into()).await.unwrap_err();
    assert!(
        matches!(error, AgentError::RecoveryRequired(_)),
        "the failed settlement must fence further mutation: {error}"
    );
    handle.stop().await.unwrap();
}

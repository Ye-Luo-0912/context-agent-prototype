use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, AttentionState, ContextAction, ContextDiagnostics, ContextEngine,
    ContextIngress, ContextItemId, ContextItemSummary, ContextKind, ContextMaintenanceReport,
    ContextMaintenanceTrigger, ContextQuery, ContextScope, ContextStateTransition, EventJournal,
    MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest, ModelRole, ModelTransport,
    RuntimeEvent, RuntimeEventEnvelope, ScopeId, ScopeKind, ToolCall, ToolDispatcher,
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolSemanticRole, ToolSpec,
};

use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use serde_json::json;
use tokio::sync::Mutex;

use crate::harness::*;

/// Records every ingest and maintain call the runtime makes, so the test can
/// assert *when* observations reached the long-term context. Also counts
/// full GC passes, so `context.collect` routing is observable. `activity`
/// is a strictly ordered log of ingests and materializations, so tests can
/// assert that a runtime directive took effect before the next model round.
#[derive(Debug, Default)]
struct RecordingContextEngine {
    ingests: Arc<Mutex<Vec<String>>>,
    maintains: Arc<Mutex<Vec<ContextMaintenanceTrigger>>>,
    gcs: Arc<Mutex<usize>>,
    activity: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        let label = match &ingress {
            ContextIngress::UserMessage { .. } => "UserMessage",
            ContextIngress::AssistantMessage { .. } => "AssistantMessage",
            ContextIngress::ToolObservation { .. } => "ToolObservation",
            ContextIngress::FocusChanged { .. } => "FocusChanged",
            ContextIngress::FocusCleared => "FocusCleared",
            ContextIngress::Pin { .. } => "Pin",
            ContextIngress::TaskCompleted { .. } => "TaskCompleted",
            ContextIngress::ContextDirective { action } => match action {
                ContextAction::CheckedFiles { .. } => "CheckedFiles",
                ContextAction::Collect => "Collect",
                ContextAction::AnchorRoots { .. } => "AnchorRoots",
                _ => "ContextDirective",
            },
            ContextIngress::WorkingSetSignal { .. } => "WorkingSetSignal",
        };
        self.ingests.lock().await.push(label.to_string());
        self.activity.lock().await.push(label.to_string());
        Ok(())
    }
    async fn gc(&self) -> AgentResult<agent_contracts::ContextGcReport> {
        *self.gcs.lock().await += 1;
        Ok(agent_contracts::ContextGcReport::default())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        self.maintains.lock().await.push(trigger);
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        self.activity.lock().await.push("Materialize".into());
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

#[derive(Debug)]
struct OkToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for OkToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "ok".into(),
            model_content: "ok from fs.read".into(),
            artifact_ref: None,
            metadata: json!({}),
        }))
    }
}

#[derive(Debug)]
struct CountingToolDispatcher {
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolDispatcher for CountingToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        OkToolDispatcher.specs()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        OkToolDispatcher.execute(request).await
    }
}

#[tokio::test]
async fn turn_frame_is_execution_stack_not_long_term_memory() {
    let context = Arc::new(RecordingContextEngine::default());
    let model = Arc::new(TwoRoundToolModel::default());
    let handle = spawn_with(
        model.clone() as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    handle.user_message("go".into()).await.unwrap();

    // Wait for the turn to persist its observations (focus + user + tool +
    // assistant).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let count = context.ingests.lock().await.len();
        if count >= 4 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not persist its observations in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // First model round: policy + Runtime Facts + Focus frame + user; no
    // tool frame yet. The implicit task's focus is a third System message.
    let requests = model.requests.lock().await;
    assert_eq!(requests.len(), 2, "two model rounds expected");
    let first = &requests[0];
    assert_eq!(
        first.iter().map(|message| message.role).collect::<Vec<_>>(),
        vec![
            ModelRole::System,
            ModelRole::System,
            ModelRole::System,
            ModelRole::User
        ]
    );
    assert!(
        first[1].content.starts_with("runtime_facts/v1"),
        "second system message is Runtime Facts, got {}",
        first[1].content
    );

    // Second round: the tool call and its result appear as protocol-paired
    // turn-frame messages with a matching tool_call_id.
    let second = &requests[1];
    let assistant = second
        .iter()
        .find(|message| message.role == ModelRole::Assistant && !message.tool_calls.is_empty())
        .expect("assistant tool-call message");
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].id, "call-1");
    let tool = second
        .iter()
        .find(|message| message.role == ModelRole::Tool)
        .expect("tool result message");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(tool.content, "ok from fs.read");
    drop(requests);

    // The observation reached the context engine only after the turn ended,
    // in ingest order: the implicit task's focus (established before the
    // message), then the user message, then the persisted tool observation,
    // then the final assistant message, then the TaskProgress CheckedFiles
    // projection before the turn-boundary GC. This dispatcher does not
    // stamp a resource path, so there is no mid-turn WorkingSetSignal.
    let ingests = context.ingests.lock().await;
    assert_eq!(
        ingests.as_slice(),
        &[
            "FocusChanged",
            "UserMessage",
            "ToolObservation",
            "AssistantMessage",
            "CheckedFiles"
        ]
    );
    drop(ingests);

    // AfterTool maintenance runs once at turn end (after both model rounds),
    // so the whole turn's observations are observed together.
    let maintains = context.maintains.lock().await;
    let after_tool = maintains
        .iter()
        .position(|trigger| *trigger == ContextMaintenanceTrigger::AfterTool)
        .expect("AfterTool maintenance must run when the turn is persisted");
    assert!(
        after_tool >= 2,
        "AfterTool must run after the model rounds, got index {after_tool}"
    );
    let after_model = maintains
        .iter()
        .position(|trigger| *trigger == ContextMaintenanceTrigger::AfterModel)
        .expect("AfterModel maintenance must run at the end");
    assert!(
        after_model > after_tool,
        "AfterModel must run after AfterTool"
    );
}

/// Records the scope lifecycle the actor drives and the scope ids carried
/// by persisted tool observations, so the test can assert that tool scopes
/// are execution frames — opened at tool start, closed when the model
/// consumes the result, and that the observations stay tagged with them.
#[derive(Debug, Default)]
struct ScopeRecordingEngine {
    opens: Arc<Mutex<Vec<(ScopeKind, ScopeId)>>>,
    closes: Arc<Mutex<Vec<ScopeId>>>,
    observation_scopes: Arc<Mutex<Vec<Option<ScopeId>>>>,
}

#[async_trait::async_trait]
impl ContextEngine for ScopeRecordingEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        if let ContextIngress::ToolObservation { scope_id, .. } = ingress {
            self.observation_scopes.lock().await.push(scope_id);
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
    async fn open_scope(&self, kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let id = ScopeId::new();
        self.opens.lock().await.push((kind, id));
        Ok(id)
    }
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.closes.lock().await.push(scope_id);
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
async fn tool_scope_opens_at_tool_start_and_closes_when_consumed() {
    let context = Arc::new(ScopeRecordingEngine::default());
    let model = Arc::new(TwoRoundToolModel::default());
    let handle = spawn_with(
        model.clone() as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();

    handle.user_message("go".into()).await.unwrap();

    // Wait for the turn to complete (two model rounds, one tool call).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut done = false;
        while let Ok(envelope) = events.try_recv() {
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                done = true;
            }
        }
        if done {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Exactly one tool frame: opened at tool start, closed once the second
    // model round consumed the result.
    let opens = context.opens.lock().await;
    assert_eq!(opens.len(), 1, "one tool call -> one tool scope");
    assert_eq!(opens[0].0, ScopeKind::Tool);
    let tool_scope = opens[0].1;
    drop(opens);

    let closes = context.closes.lock().await;
    assert_eq!(
        closes.as_slice(),
        &[tool_scope],
        "the consumed tool scope must close with its own id"
    );
    drop(closes);

    // The persisted observation is tagged with the tool frame even though
    // persistence happens at turn end, after the frame closed.
    let observation_scopes = context.observation_scopes.lock().await;
    assert_eq!(
        observation_scopes.as_slice(),
        &[Some(tool_scope)],
        "the tool observation must carry its producing scope"
    );
}

#[tokio::test]
async fn tool_operation_identity_is_published_after_core_admission_before_tool_start() {
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(TestContextEngine),
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut accepted = None;
    let mut accepted_position = None;
    let mut started_position = None;
    let mut position = 0_usize;
    while tokio::time::Instant::now() < deadline {
        if let Ok(envelope) = tokio::time::timeout(Duration::from_millis(50), events.recv()).await {
            position += 1;
            match envelope.unwrap().event {
                RuntimeEvent::OperationAccepted { snapshot } => {
                    assert!(matches!(
                        snapshot.state,
                        agent_contracts::OperationState::Accepted
                    ));
                    accepted_position = Some(position);
                    accepted = Some(snapshot);
                }
                RuntimeEvent::ToolStarted { .. } => started_position = Some(position),
                RuntimeEvent::TurnCompleted => break,
                _ => {}
            }
        }
    }
    let snapshot = accepted.expect("tool operation must publish its WAL-backed identity");
    assert_eq!(snapshot.identity.call_id, "call-1");
    assert_eq!(snapshot.identity.tool_name, "fs.read");
    assert!(snapshot.identity.scope_id.is_some());
    assert!(
        accepted_position < started_position,
        "OperationAccepted must precede ToolStarted"
    );
    let queried = handle
        .query_operation(snapshot.identity.operation_id)
        .await
        .unwrap();
    assert!(matches!(
        queried,
        agent_contracts::OperationQueryResult::Found { snapshot: retained }
            if retained.identity == snapshot.identity
    ));
}

#[derive(Debug, Default)]
struct FailOperationAcceptedJournal {
    accepted: std::sync::Mutex<Option<agent_contracts::OperationSnapshot>>,
    persisted_sequences: std::sync::Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl EventJournal for FailOperationAcceptedJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if let RuntimeEvent::OperationAccepted { snapshot } = &envelope.event {
            *self.accepted.lock().unwrap() = Some((**snapshot).clone());
            return Err(AgentError::Storage(
                "simulated operation-accepted journal failure".into(),
            ));
        }
        self.persisted_sequences.lock().unwrap().push(envelope.seq);
        Ok(())
    }
}

#[tokio::test]
async fn operation_accepted_audit_failure_closes_scope_without_dispatch() {
    let context = Arc::new(ScopeRecordingEngine::default());
    let executions = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailOperationAcceptedJournal::default());
    let services = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(TwoRoundToolModel::default()),
        Arc::new(CountingToolDispatcher {
            executions: executions.clone(),
        }),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(journal.clone()),
    ));
    let (handle, _task) = spawn_runtime(services);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("go".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_failure = false;
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if matches!(
                envelope.event,
                RuntimeEvent::Error { ref message }
                    if message.contains("simulated operation-accepted journal failure")
            ) {
                saw_failure = true;
            }
        }
        if saw_failure && !context.closes.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        saw_failure,
        "the failed discovery event must remain observable"
    );
    let opens = context.opens.lock().await;
    assert_eq!(opens.len(), 1, "one attempted tool must open one scope");
    let tool_scope = opens[0].1;
    drop(opens);
    assert_eq!(
        context.closes.lock().await.as_slice(),
        &[tool_scope],
        "the admitted-but-undispatched tool scope must be closed"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "dropping the one-shot permit must prevent tool dispatch"
    );

    let accepted = journal
        .accepted
        .lock()
        .unwrap()
        .clone()
        .expect("the failing journal must observe the admitted snapshot");
    let query = handle
        .query_operation(accepted.identity.operation_id)
        .await
        .unwrap();
    assert!(matches!(
        query,
        agent_contracts::OperationQueryResult::Found { snapshot }
            if matches!(
                snapshot.state,
                agent_contracts::OperationState::Terminal {
                    terminal: agent_contracts::OperationTerminal::CancelledBeforeCommit,
                    ..
                }
            )
    ));
    assert!(matches!(
        handle.user_message("must remain fenced".into()).await,
        Err(AgentError::RecoveryRequired(_))
    ));
    let persisted = journal.persisted_sequences.lock().unwrap();
    assert_eq!(
        persisted.as_slice(),
        &(1..=u64::try_from(persisted.len()).unwrap()).collect::<Vec<_>>(),
        "a rejected event append must not leave a durable sequence gap"
    );
}

/// A context engine that records the scopes the actor closes and returns a
/// fixed promotion transition from `close_scope`, so the test can assert
/// that the runtime publishes the close as an auditable event instead of
/// discarding it.
#[derive(Debug, Default)]
struct PublishingScopeEngine {
    closes: Arc<Mutex<Vec<ScopeId>>>,
}

#[async_trait::async_trait]
impl ContextEngine for PublishingScopeEngine {
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
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        self.closes.lock().await.push(scope_id);
        Ok(vec![ContextStateTransition {
            item_id: ContextItemId::new(),
            kind: ContextKind::Note,
            scope: ContextScope::Turn,
            from: AttentionState::Archived,
            to: AttentionState::Active,
            turn: 0,
            reason: "promoted by tool scope close".into(),
        }])
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

/// A context engine whose `close_scope` always fails, so the test can assert
/// that a failed tool-frame close is surfaced as an `Error` event instead of
/// being swallowed by `let _ =`.
#[derive(Debug, Default)]
struct FailingCloseScopeEngine;

#[async_trait::async_trait]
impl ContextEngine for FailingCloseScopeEngine {
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
        Err(AgentError::Context("simulated close failure".into()))
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

/// Wait for the turn to complete (two model rounds, one tool call),
/// collecting every event seen on the way so the caller can assert on
/// events that precede `TurnCompleted` (a broadcast receiver drops events
/// once they are consumed, so a separate post-completion read would miss
/// them).
async fn wait_for_turn_completion(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<RuntimeEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let mut done = false;
        while let Ok(envelope) = events.try_recv() {
            done |= matches!(envelope.event, RuntimeEvent::TurnCompleted);
            seen.push(envelope.event);
        }
        if done {
            return seen;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn did not complete in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The tool-frame close is published as an auditable result: the runtime
/// emits `ToolScopeClosed` with the transitions the close produced instead
/// of discarding them.
#[tokio::test]
async fn tool_scope_close_publishes_its_transitions() {
    let context = Arc::new(PublishingScopeEngine::default());
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    let seen = wait_for_turn_completion(&mut events).await;

    let (scope_id, transitions) = seen
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolScopeClosed {
                scope_id,
                transitions,
            } => Some((*scope_id, transitions)),
            _ => None,
        })
        .expect("a ToolScopeClosed event must be published");
    assert!(
        !transitions.is_empty(),
        "the transitions produced by the close must ride the event"
    );
    let closes = context.closes.lock().await;
    assert_eq!(
        closes.as_slice(),
        &[scope_id],
        "the closed scope id must match the scope the engine actually closed"
    );
}

/// A failed tool-frame close is surfaced as an `Error` event instead of
/// being silently discarded.
#[tokio::test]
async fn tool_scope_close_failure_is_published_as_an_error() {
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        Arc::new(FailingCloseScopeEngine),
        Arc::new(OkToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    let seen = wait_for_turn_completion(&mut events).await;

    let error = seen
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("a failed tool-frame close must publish an Error event");
    assert!(
        error.contains("closing tool scope"),
        "the error must name the failing close, got: {error}"
    );
}

/// Records every `ContextIngress` the actor sends, so the test can assert
/// that a tool commit signals its discovered entities *before* the turn-end
/// observation is persisted.
#[derive(Debug, Default)]
struct IngestRecordingEngine {
    ingests: Arc<Mutex<Vec<ContextIngress>>>,
}

#[async_trait::async_trait]
impl ContextEngine for IngestRecordingEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.ingests.lock().await.push(ingress);
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

/// A read-only tool whose bounded output names a discovered entity, so the
/// test can assert the runtime signals it before the next model round.
struct EntitySignalingDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for EntitySignalingDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: true,
            summary: "found it".into(),
            model_content: "discovered AuthService.rs".into(),
            artifact_ref: None,
            metadata: json!({"path": "src/AuthService.rs"}),
        }))
    }
}

/// A tool commit signals the entities its output discovered to the context
/// engine — before the observation body is persisted at turn end — so the
/// very next model round can recall evidence without duplicating the tool
/// body.
#[tokio::test]
async fn tool_commit_signals_discovered_entities_before_the_next_round() {
    let context = Arc::new(IngestRecordingEngine::default());
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(EntitySignalingDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    wait_for_turn_completion(&mut events).await;

    let ingests = context.ingests.lock().await;
    let signal = ingests
        .iter()
        .find_map(|ingress| match ingress {
            ContextIngress::WorkingSetSignal { resources, .. } => {
                resources.first().map(|touch| touch.path.clone())
            }
            _ => None,
        })
        .expect("a WorkingSetSignal must be sent at tool commit");
    assert!(
        signal.contains("AuthService.rs"),
        "the tool's stamped path must be signaled, got: {signal}"
    );
    let signal_pos = ingests
        .iter()
        .position(|ingress| matches!(ingress, ContextIngress::WorkingSetSignal { .. }))
        .expect("the signal must be present");
    let observation_pos = ingests
        .iter()
        .position(|ingress| matches!(ingress, ContextIngress::ToolObservation { .. }))
        .expect("the observation must be persisted at turn end");
    assert!(
        signal_pos < observation_pos,
        "the signal must reach the engine before the observation body is \
         persisted at turn end"
    );
}

/// Failed execution results stay on the TurnFrame. Candidate paths in the
/// error body must not become a WorkingSetSignal / hot-entity merge.
struct FailedEntitySignalingDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for FailedEntitySignalingDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "fs.read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(ToolOutput {
            call_id: request.call.id,
            tool_name: request.call.name,
            ok: false,
            summary: "no_exact_match".into(),
            model_content: "candidate:\nsrc/foo.rs\nsrc/bar.rs".into(),
            artifact_ref: None,
            metadata: json!({"failure_class": "no_exact_match"}),
        }))
    }
}

#[tokio::test]
async fn failed_tool_commit_does_not_signal_hot_entities() {
    let context = Arc::new(IngestRecordingEngine::default());
    let handle = spawn_with(
        Arc::new(TwoRoundToolModel::default()) as Arc<dyn ModelTransport>,
        context.clone() as Arc<dyn ContextEngine>,
        Arc::new(FailedEntitySignalingDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("go".into()).await.unwrap();
    wait_for_turn_completion(&mut events).await;

    let ingests = context.ingests.lock().await;
    assert!(
        ingests
            .iter()
            .all(|ingress| !matches!(ingress, ContextIngress::WorkingSetSignal { .. })),
        "failed tool output must not heat the working set, got: {ingests:?}"
    );
    assert!(
        ingests
            .iter()
            .any(|ingress| matches!(ingress, ContextIngress::ToolObservation { .. })),
        "the failed observation must still persist at turn end"
    );
}

// ---------------------------------------------------------------------------
// Context directive routing: a tool's `RuntimeDirective` is executed at
// operation-commit time — right after any staged effect, before the result
// enters the turn frame — so a "manual collect now" is actually now and a
// lease lands before the next model round, not at turn end. Tools never
// touch the engine — the runtime routes.
// ---------------------------------------------------------------------------

/// Emits one tool call (`name`) with the given arguments, then plain text.
#[derive(Debug)]
struct DirectiveModel {
    tool_name: &'static str,
    rounds: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelTransport for DirectiveModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: self.tool_name.into(),
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

/// Serves the context meta-tools: each returns a `RuntimeDirective` with the
/// matching `ContextAction`, exactly like the real `context.*` tools.
#[derive(Debug)]
struct DirectiveToolDispatcher;

#[async_trait::async_trait]
impl ToolDispatcher for DirectiveToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "context.lease".into(),
                description: "lease an item".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.collect".into(),
                description: "run GC now".into(),
                input_schema: json!({"type": "object"}),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
        ]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let directive = match request.call.name.as_str() {
            "context.lease" => {
                agent_contracts::RuntimeDirective::Context(agent_contracts::ContextAction::Lease {
                    item_id: agent_contracts::ContextItemId::new(),
                    turns: 3,
                })
            }
            "context.collect" => {
                agent_contracts::RuntimeDirective::Context(agent_contracts::ContextAction::Collect)
            }
            other => {
                return Err(agent_contracts::AgentError::Tool(format!(
                    "unknown tool: {other}"
                )));
            }
        };
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "directive queued".into(),
                model_content: "directive queued".into(),
                artifact_ref: None,
                metadata: json!({}),
            },
            directive,
        })
    }
}

#[tokio::test]
async fn actor_routes_lease_directive_into_the_context_engine() {
    let context = Arc::new(RecordingContextEngine::default());
    let handle = spawn_with(
        Arc::new(DirectiveModel {
            tool_name: "context.lease",
            rounds: AtomicUsize::new(0),
        }),
        context.clone(),
        Arc::new(DirectiveToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("lease something".into()).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter().any(|e| e.contains("TurnCompleted")),
        "the turn must complete; saw: {seen:?}"
    );

    let ingests = context.ingests.lock().await;
    assert!(
        ingests.iter().any(|label| label == "ContextDirective"),
        "the directive must be routed as a ContextDirective ingest, got: {ingests:?}"
    );
    // The directive executes at operation-commit time: it must land BEFORE
    // the observation is persisted at turn end — "now", not "later".
    let directive_index = ingests.iter().position(|label| label == "ContextDirective");
    let observation_index = ingests.iter().position(|label| label == "ToolObservation");
    assert!(
        directive_index.is_some()
            && observation_index.is_some()
            && directive_index < observation_index,
        "the directive must be executed before the observation is persisted, got: {ingests:?}"
    );
    drop(ingests);

    // Stronger timing invariant: the directive must take effect before the
    // NEXT model round materializes, not just before turn-end persistence.
    // The model calls the tool on round 0 and finishes on round 1, so the
    // second materialization happens after the directive — prove it by
    // ordering on the shared activity log.
    let activity = context.activity.lock().await;
    let directive_index = activity
        .iter()
        .position(|entry| entry == "ContextDirective");
    let second_materialize = activity
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.as_str() == "Materialize")
        .nth(1)
        .map(|(index, _)| index);
    assert!(
        directive_index.is_some()
            && second_materialize.is_some()
            && directive_index < second_materialize,
        "a ContextAction must be effective before the next model round, got: {activity:?}"
    );
}

#[tokio::test]
async fn actor_routes_collect_directive_into_a_full_gc_pass() {
    let context = Arc::new(RecordingContextEngine::default());
    let handle = spawn_with(
        Arc::new(DirectiveModel {
            tool_name: "context.collect",
            rounds: AtomicUsize::new(0),
        }),
        context.clone(),
        Arc::new(DirectiveToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("collect now".into()).await.unwrap();
    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
            if matches!(envelope.event, RuntimeEvent::TurnCompleted) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter().any(|e| e.contains("TurnCompleted")),
        "the turn must complete; saw: {seen:?}"
    );

    // `context.collect` bypasses ingest entirely: it is the one directive
    // the runtime executes itself (it owns the GC pass). The turn-boundary
    // CheckedFiles projection is a different ingest.
    let ingests = context.ingests.lock().await;
    assert!(
        !ingests.iter().any(|label| label == "Collect"),
        "collect is not an ingest directive, got: {ingests:?}"
    );
    drop(ingests);
    let gcs = context.gcs.lock().await;
    assert_eq!(
        *gcs, 2,
        "the manual collect adds one GC pass on top of the regular turn-boundary pass"
    );
}

// ---------------------------------------------------------------------------
// Audit failures must be propagated, not silent: a state change must never
// outrun its journal event (CTX-09 audit-failure propagation).
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FailBeforeModelJournal;

#[async_trait::async_trait]
impl EventJournal for FailBeforeModelJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(
            envelope.event,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::BeforeModel,
                ..
            }
        ) {
            return Err(AgentError::Storage(
                "simulated before-model journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct FailGcEventJournal;

#[async_trait::async_trait]
impl EventJournal for FailGcEventJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        if matches!(envelope.event, RuntimeEvent::ContextGc { .. }) {
            return Err(AgentError::Storage(
                "simulated gc-event journal failure".into(),
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn before_model_audit_failure_fences_the_turn() {
    // The BeforeModel maintenance state change landed, but its
    // ContextMaintained audit event did not: the turn must be fenced —
    // the model is never called and no TurnCompleted is emitted, so state
    // cannot silently outrun its journal event.
    let model = Arc::new(DirectiveModel {
        tool_name: "context.collect",
        rounds: AtomicUsize::new(0),
    });
    let rounds_ref = model.clone();
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        Arc::new(RecordingContextEngine::default()),
        model,
        Arc::new(DirectiveToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailBeforeModelJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("hello".into()).await.unwrap();

    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
        }
        if seen.iter().any(|e| e.contains("Error")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter()
            .any(|e| e.contains("simulated before-model journal failure")),
        "the audit failure must surface as an Error event, saw: {seen:?}"
    );
    assert_eq!(
        rounds_ref.rounds.load(Ordering::SeqCst),
        0,
        "the fenced turn must never reach the model"
    );
    assert!(
        !seen.iter().any(|e| e.contains("TurnCompleted")),
        "no TurnCompleted may be emitted for a turn whose audit event failed, saw: {seen:?}"
    );
}

#[tokio::test]
async fn collect_audit_failure_is_not_silent() {
    // `context.collect` runs a full GC pass at operation-commit time; when
    // the resulting ContextGc audit event cannot be journaled, the runtime
    // must surface the failure as an Error event instead of dropping it.
    let context = Arc::new(RecordingContextEngine::default());
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        context.clone(),
        Arc::new(DirectiveModel {
            tool_name: "context.collect",
            rounds: AtomicUsize::new(0),
        }),
        Arc::new(DirectiveToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        Some(Arc::new(FailGcEventJournal)),
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let mut events = handle.subscribe();
    handle.start().await.unwrap();
    handle.user_message("collect now".into()).await.unwrap();

    let mut seen: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            seen.push(format!("{:?}", envelope.event));
        }
        if seen.iter().any(|e| e.contains("RecoveryRequired")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        seen.iter()
            .any(|e| e.contains("simulated gc-event journal failure")),
        "the collect audit failure must surface as an Error event, saw: {seen:?}"
    );
    // The GC state change itself still happened (the failure is the event,
    // not the pass): the turn-boundary GC plus the manual collect.
    let gcs = context.gcs.lock().await;
    assert_eq!(*gcs, 2, "both GC passes still ran");
    // And the same journal fault at the turn-boundary GC audit is not
    // silent either: the turn commit fails and the runtime demands
    // recovery instead of claiming a commit whose audit never landed.
    assert!(
        seen.iter().any(|e| e.contains("RecoveryRequired")),
        "a failed GC audit event must fence the turn into recovery, saw: {seen:?}"
    );
}

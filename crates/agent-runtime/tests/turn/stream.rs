use std::{sync::Arc, time::Duration};

use agent_contracts::{
    AgentError, AgentResult, EventJournal, ModelCapabilities, ModelChunk, ModelEventSink,
    ModelOutput, ModelRequest, ModelTransport, OperationId, RuntimeEvent, RuntimeEventEnvelope,
    TurnCancelAck, TurnCancellationReason,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeServices, spawn_runtime};
use agent_workspace::Workspace;
use tokio::sync::Notify;

use crate::harness::*;

#[derive(Debug)]
struct HangingModel;

#[async_trait::async_trait]
impl ModelTransport for HangingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        request.cancel.cancelled().await;
        Err(agent_contracts::AgentError::Cancelled)
    }
}

/// COST-7 (R2-11): completes WITH usage shortly after the cancellation
/// fires — the provider had already answered, but the turn has moved on.
/// The late result must not advance the task, and its cost must be counted
/// exactly once (the cancellation's unknown row stands in; a second,
/// double-counted row must not appear).
#[derive(Debug)]
struct LateUsageModel;

#[async_trait::async_trait]
impl ModelTransport for LateUsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        request.cancel.cancelled().await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        Ok(ModelOutput {
            content: "too late".into(),
            tool_calls: Vec::new(),
            usage: agent_contracts::ModelUsage {
                input_tokens: Some(321),
                output_tokens: Some(78),
                ..Default::default()
            },
        })
    }
}

/// Deliberately ignores cancellation until the test releases it. Real
/// provider transports can have the same short-lived lag while a request is
/// unwinding, so actor shutdown may not assume the model future is gone.
#[derive(Debug)]
struct CancellationLagModel {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl ModelTransport for CancellationLagModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(AgentError::Cancelled)
    }
}

/// Emits one live-only delta, then stays in-flight until cancellation.
#[derive(Debug)]
struct StreamingHangingModel;

#[async_trait::async_trait]
impl ModelTransport for StreamingHangingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            ..ModelCapabilities::default()
        }
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        unreachable!("streaming model should be driven through complete_stream")
    }

    async fn complete_stream(
        &self,
        request: ModelRequest,
        sink: &dyn ModelEventSink,
    ) -> AgentResult<ModelOutput> {
        sink.on_chunk(ModelChunk::TextDelta {
            delta: "partial".into(),
        })
        .await?;
        request.cancel.cancelled().await;
        Err(AgentError::Cancelled)
    }
}

/// Records every ingest and maintain call the runtime makes, so the test can
/// assert *when* observations reached the long-term context. Also counts
/// full GC passes, so `context.collect` routing is observable. `activity`
/// is a strictly ordered log of ingests and materializations, so tests can

#[derive(Debug, Default)]
struct SequenceJournal {
    envelopes: std::sync::Mutex<Vec<RuntimeEventEnvelope>>,
}

#[async_trait::async_trait]
impl EventJournal for SequenceJournal {
    async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
        self.envelopes.lock().unwrap().push(envelope.clone());
        Ok(())
    }
}

#[tokio::test]
async fn actor_streams_model_deltas_to_subscribers() {
    let journal = Arc::new(SequenceJournal::default());
    let handle = spawn_with_journal(Arc::new(StreamingModel), journal.clone()).await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut deltas = Vec::new();
    let mut delta_cursors = Vec::new();
    let mut model_started_cursor = None;
    let mut final_content = None;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            let cursor = envelope.seq;
            match envelope.event {
                RuntimeEvent::ModelStarted { .. } => model_started_cursor = Some(cursor),
                RuntimeEvent::ModelDelta {
                    delta,
                    operation_id,
                    ..
                } => {
                    // Every delta must belong to the round that emitted it:
                    // the fence identity is present, not defaulted.
                    assert!(operation_id != OperationId::default());
                    deltas.push(delta);
                    delta_cursors.push(cursor);
                }
                RuntimeEvent::AssistantMessage { content } => final_content = Some(content),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if turn_completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(turn_completed, "the turn must complete");
    assert_eq!(deltas, vec!["Hello ".to_string(), "world".to_string()]);
    assert_eq!(
        delta_cursors,
        vec![model_started_cursor.unwrap(); 2],
        "live deltas repeat their opening durable cursor; their operation identity is the fence"
    );
    assert_eq!(final_content.as_deref(), Some("Hello world"));

    handle.stop().await.unwrap();
    let durable = journal.envelopes.lock().unwrap();
    assert!(
        durable
            .iter()
            .all(|envelope| !matches!(envelope.event, RuntimeEvent::ModelDelta { .. })),
        "streaming deltas are live-only and must never enter the recovery trace"
    );
    for (expected, envelope) in (1u64..).zip(durable.iter()) {
        assert_eq!(
            envelope.seq, expected,
            "live-only deltas must not consume durable journal sequence numbers"
        );
    }
    assert!(
        matches!(
            durable.last().map(|envelope| &envelope.event),
            Some(RuntimeEvent::RunCompleted)
        ),
        "the contiguous healthy trace must include terminal shutdown"
    );
}

#[tokio::test]
async fn streamed_cancellation_and_shutdown_leave_a_contiguous_recovery_trace() {
    let journal = Arc::new(SequenceJournal::default());
    let handle = spawn_with_journal(Arc::new(StreamingHangingModel), journal.clone()).await;
    let mut events = handle.subscribe();
    handle
        .user_message("cancel after streaming".into())
        .await
        .unwrap();

    let (model_started_cursor, delta_cursor): (u64, u64) =
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut started = None;
            loop {
                let envelope = events.recv().await.unwrap();
                match envelope.event {
                    RuntimeEvent::ModelStarted { .. } => started = Some(envelope.seq),
                    RuntimeEvent::ModelDelta { .. } => {
                        break (
                            started.expect("ModelStarted precedes its deltas"),
                            envelope.seq,
                        );
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("stream never started");

    handle.cancel_turn().await.unwrap();
    handle.stop().await.unwrap();

    let durable = journal.envelopes.lock().unwrap();
    assert_eq!(delta_cursor, model_started_cursor);
    assert!(
        durable
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::TurnCancelled { .. }))
    );
    assert!(
        durable
            .iter()
            .any(|envelope| matches!(envelope.event, RuntimeEvent::RunCompleted))
    );
    assert!(
        durable
            .iter()
            .all(|envelope| !matches!(envelope.event, RuntimeEvent::TurnCompleted)),
        "cancellation must not become a successful commit marker"
    );
    for (expected, envelope) in (1u64..).zip(durable.iter()) {
        assert_eq!(envelope.seq, expected);
    }
}

#[tokio::test]
async fn cancelled_model_future_does_not_retain_runtime_services_after_shutdown() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let tools = Arc::new(TestToolDispatcher);
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).await.unwrap();
    let services = Arc::new(
        RuntimeServices::new(
            CoreAuthorityConfig::default(),
            Arc::new(TestContextEngine),
            Arc::new(CancellationLagModel {
                entered: entered.clone(),
                release: release.clone(),
            }),
            tools.clone(),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        )
        .with_artifact_workspace(Arc::new(workspace)),
    );
    let (handle, actor_task) = spawn_runtime(services.clone());
    drop(services);
    handle.start().await.unwrap();
    handle
        .user_message("start slow request".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("model request never started");

    handle.stop().await.unwrap();
    actor_task.await.unwrap();

    assert_eq!(
        Arc::strong_count(&tools),
        1,
        "a cancelled provider future must not retain the tool/workspace service bundle"
    );
    let reopened = Workspace::open(directory.path())
        .await
        .expect("shutdown must release the workspace effect-journal lock");
    drop(reopened);
    release.notify_waiters();
}

/// Returns one plain text answer with a fixed provider usage report.
#[derive(Debug)]
struct UsageModel(u64, u64);

#[async_trait::async_trait]
impl ModelTransport for UsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Ok(ModelOutput {
            content: "done".into(),
            tool_calls: Vec::new(),
            usage: agent_contracts::ModelUsage {
                input_tokens: Some(self.0),
                output_tokens: Some(self.1),
                ..Default::default()
            },
        })
    }
}

#[tokio::test]
async fn actor_reports_provider_usage_via_model_used() {
    let handle = spawn_with(
        Arc::new(UsageModel(321, 78)),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut used: Option<(u64, u64)> = None;
    let mut turn_completed = false;
    for _ in 0..500 {
        if let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                    ..
                } => used = Some((input_tokens, output_tokens)),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if turn_completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(turn_completed, "the turn must complete");
    assert_eq!(
        used,
        Some((321, 78)),
        "the provider-reported usage must reach subscribers as ModelUsed"
    );
}

#[tokio::test]
async fn actor_cancels_hanging_model_cleanly() {
    let handle = spawn_with(
        Arc::new(HangingModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    // Give the model round time to start and block inside the model call.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let acknowledgement = handle.cancel_turn().await.unwrap();

    let (ack_turn_id, ack_generation) = match acknowledgement {
        TurnCancelAck::Cancelled {
            turn_id,
            effective_generation,
            ..
        } => (turn_id, effective_generation),
        TurnCancelAck::NoActiveTurn => panic!("the hanging turn must still be active"),
    };
    let mut cancelled_event = None;
    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::TurnCancelled {
                    turn_id,
                    effective_generation,
                    reason,
                    ..
                } => cancelled_event = Some((turn_id, effective_generation, reason)),
                RuntimeEvent::TurnCompleted => turn_completed = true,
                _ => {}
            }
        }
        if cancelled_event.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        cancelled_event,
        Some((
            ack_turn_id,
            ack_generation,
            TurnCancellationReason::Requested
        )),
        "the durable event and caller acknowledgement must describe the same cancellation"
    );
    assert!(
        !turn_completed,
        "cancellation must never masquerade as a successful turn commit"
    );
}

/// COST-7 (R3-12): fails AFTER the provider reported usage for the
/// attempt — the typed failure carries the known counters.
#[derive(Debug)]
struct FailedWithUsageModel;

#[async_trait::async_trait]
impl ModelTransport for FailedWithUsageModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        Err(AgentError::failed_with_usage(
            agent_contracts::ModelUsage {
                input_tokens: Some(321),
                output_tokens: Some(78),
                attempts: 1,
                ..agent_contracts::ModelUsage::default()
            },
            AgentError::Model("generation failed after billing".into()),
        ))
    }
}

/// COST-7 (R3-12): a failed MODEL round whose provider usage arrived keeps
/// the REAL counters under their honest identity on the main lane — the
/// failure no longer downgrades known evidence to an unknown row.
#[tokio::test]
async fn a_failed_round_with_reported_usage_keeps_the_real_counters() {
    let handle = spawn_with(
        Arc::new(FailedWithUsageModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    let mut model_used = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            if let RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
                usage_identity,
                role,
                ..
            } = envelope.event
            {
                model_used = Some((input_tokens, output_tokens, usage_identity, role));
            }
        }
        if model_used.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (input, output, identity, role) =
        model_used.expect("the failed round must still produce a usage row");
    assert_eq!((input, output), (321, 78), "the real counters survive");
    assert_eq!(
        identity,
        agent_contracts::UsageIdentity::Observed,
        "a full provider report stays observed, not unknown"
    );
    assert_eq!(role, agent_contracts::ModelCallRole::Main);
}

/// COST-7 (R2-11): cancel and a late usage-bearing completion arrive for
/// the same round. The account carries exactly one row — the cancellation's
/// unknown main-lane row — and the stale completion supplements nothing
/// twice, does not advance the turn, and does not emit a second observed
/// row for the same operation.
#[tokio::test]
async fn cancel_and_late_completion_count_one_cost_exactly_once() {
    let handle = spawn_with(
        Arc::new(LateUsageModel),
        Arc::new(TestContextEngine),
        Arc::new(TestToolDispatcher),
    )
    .await;
    let mut events = handle.subscribe();
    handle.user_message("hello".into()).await.unwrap();

    // Cancel while the model call is parked on the cancellation token.
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel_turn().await.unwrap();

    // Let the late usage-bearing completion land and be processed stale.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut model_used_rows: Vec<(u64, u64, &'static str, &'static str)> = Vec::new();
    let mut turn_completed = false;
    while let Ok(envelope) = events.try_recv() {
        match envelope.event {
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
                usage_identity,
                role,
                ..
            } => {
                let identity = match usage_identity {
                    agent_contracts::UsageIdentity::Observed => "observed",
                    agent_contracts::UsageIdentity::Estimated => "estimated",
                    agent_contracts::UsageIdentity::Unknown => "unknown",
                };
                let role_name = match role {
                    agent_contracts::ModelCallRole::Main => "main",
                    agent_contracts::ModelCallRole::Maintenance => "maintenance",
                };
                model_used_rows.push((input_tokens, output_tokens, identity, role_name));
            }
            RuntimeEvent::TurnCompleted => turn_completed = true,
            _ => {}
        }
    }
    assert!(!turn_completed, "the cancelled turn must not complete");
    assert_eq!(
        model_used_rows.len(),
        1,
        "one round costs one row: got {model_used_rows:?}"
    );
    let (input, output, identity, role) = model_used_rows[0];
    assert_eq!(
        (input, output),
        (0, 0),
        "the unknown row carries no invented counters"
    );
    assert_eq!(
        identity, "unknown",
        "the cancelled round's evidence is unknown"
    );
    assert_eq!(role, "main", "the row names the main lane");
}

//! W04: the real dynamic engine may await a compactor inside UserMessage
//! ingestion, before its UserInput maintenance pass can even start.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, BoundedCompactor, CompactionOutput, CompactionReason,
    CompactionRequest, ContextEngine, ContextMaintenanceTrigger, InputLifecycle, ModelCapabilities,
    ModelOutput, ModelRequest, ModelTransport, RuntimeEvent, RuntimeEventEnvelope, TurnCancelAck,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, spawn_runtime};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use tokio::sync::{Notify, broadcast};

const NEW_INPUT: &str = "keep AuthService.rs stable while changing the second login phase";

#[derive(Default)]
struct GatedCompactor {
    entered: Notify,
    release: Notify,
    aborted: AtomicBool,
    calls: AtomicUsize,
    source: Mutex<String>,
}

struct PendingCompact<'a> {
    aborted: &'a AtomicBool,
    completed: bool,
}

impl Drop for PendingCompact<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.aborted.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl BoundedCompactor for GatedCompactor {
    async fn compact(&self, request: CompactionRequest) -> AgentResult<CompactionOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.source.lock().unwrap() = request.source;
        let mut pending = PendingCompact {
            aborted: &self.aborted,
            completed: false,
        };
        self.entered.notify_one();
        self.release.notified().await;
        pending.completed = true;
        Ok(CompactionOutput {
            text: "[episode card] keep the original unversioned ping constraint".into(),
            input_tokens: 17,
            output_tokens: 5,
            usage_identity: agent_contracts::UsageIdentity::Observed,
            ..Default::default()
        })
    }
}

#[derive(Debug, Default)]
struct RecordingModel {
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait::async_trait]
impl ModelTransport for RecordingModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        self.requests.lock().unwrap().push(request);
        Ok(ModelOutput {
            content: "The current directive remains ready for operator review.".into(),
            tool_calls: Vec::new(),
            usage: Default::default(),
        })
    }
}

async fn completed(events: &mut broadcast::Receiver<RuntimeEventEnvelope>) -> Vec<RuntimeEvent> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut received = Vec::new();
        loop {
            received.push(events.recv().await.unwrap().event);
            if matches!(received.last(), Some(RuntimeEvent::TurnCompleted)) {
                return received;
            }
        }
    })
    .await
    .expect("turn must finish without waiting for the gated compactor")
}

struct Fixture {
    handle: RuntimeHandle,
    engine: Arc<SimpleContextEngine>,
    compactor: Arc<GatedCompactor>,
    model: Arc<RecordingModel>,
    events: broadcast::Receiver<RuntimeEventEnvelope>,
    before: serde_json::Value,
    original: String,
    _store: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let store = tempfile::tempdir().unwrap();
        let compactor = Arc::new(GatedCompactor::default());
        let engine = Arc::new(
            SimpleContextEngine::new(SimpleContextConfig {
                // Only shorten the episode for the fixture. A real pinned
                // semantic constraint below qualifies the episode for LLM
                // distillation; force_episode_llm_distill stays false.
                episode_max_user_turns: 2,
                episode_rotate_threshold: 0.0,
                context_store_dir: Some(store.path().to_path_buf()),
                ..SimpleContextConfig::default()
            })
            .with_compactor(compactor.clone()),
        );
        let model = Arc::new(RecordingModel::default());
        let services = Arc::new(RuntimeServices::new(
            CoreAuthorityConfig::default(),
            engine.clone(),
            model.clone(),
            Arc::new(super::harness::TestToolDispatcher),
            Arc::new(PolicyApprovalGate::read_only()),
            None,
        ));
        let (handle, _) = spawn_runtime(services);
        handle.start().await.unwrap();
        handle
            .set_focus("keep AuthService.rs stable".into())
            .await
            .unwrap();
        let mut events = handle.subscribe();
        let original = format!(
            "keep AuthService.rs stable: {} ORIGINAL_TAIL_CONSTRAINT",
            "preserve the login contract; ".repeat(25)
        );
        handle.user_message(original.clone()).await.unwrap();
        completed(&mut events).await;
        handle
            .pin("Keep the original unversioned ping wire constraint".into())
            .await
            .unwrap();
        let before = engine.checkpoint().await.unwrap();
        Self {
            handle,
            engine,
            compactor,
            model,
            events,
            before,
            original,
            _store: store,
        }
    }

    async fn enter_ingest(&self) -> tokio::task::JoinHandle<AgentResult<()>> {
        let handle = self.handle.clone();
        let input = tokio::spawn(async move { handle.user_message(NEW_INPUT.into()).await });
        tokio::time::timeout(Duration::from_secs(3), self.compactor.entered.notified())
            .await
            .expect("real Simple engine did not enter episode compaction");
        assert_eq!(self.compactor.calls.load(Ordering::SeqCst), 1);
        assert!(
            !input.is_finished(),
            "admission cannot succeed while ingest is pending"
        );
        input
    }
}

#[tokio::test]
async fn cancel_during_dynamic_ingest_restores_context_and_original_directive() {
    let mut f = Fixture::new().await;
    let input = f.enter_ingest().await;
    assert!(
        f.compactor
            .source
            .lock()
            .unwrap()
            .contains("ORIGINAL_TAIL_CONSTRAINT")
    );
    let handle = f.handle.clone();
    let mut cancelling = tokio::spawn(async move { handle.cancel_turn().await });
    let receipt = tokio::time::timeout(Duration::from_secs(2), &mut cancelling).await;
    if receipt.is_err() {
        // Make the pre-fix failure bounded: release the engine and shut the
        // actor down before failing, without leaking a blocked task.
        f.compactor.release.notify_one();
        let _ = input.await;
        let _ = cancelling.await;
        let _ = f.handle.stop().await;
        panic!("cancel_turn was blocked by SimpleContextEngine::ingest compaction");
    }
    assert!(matches!(
        receipt.unwrap().unwrap().unwrap(),
        TurnCancelAck::Cancelled { .. }
    ));
    assert!(matches!(input.await.unwrap(), Err(AgentError::Cancelled)));
    assert!(
        f.compactor.aborted.load(Ordering::SeqCst),
        "ACK must follow compactor drop"
    );
    assert_eq!(
        f.model.requests.lock().unwrap().len(),
        1,
        "cancelled input must not reach the model"
    );
    assert_eq!(
        f.engine.checkpoint().await.unwrap(),
        f.before,
        "rollback must restore the whole pre-ingest context"
    );
    f.handle.continue_active_task().await.unwrap();
    completed(&mut f.events).await;
    {
        let requests = f.model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|m| m.content.contains(&f.original))
        );
        assert!(
            requests[1]
                .messages
                .iter()
                .all(|m| !m.content.contains(NEW_INPUT))
        );
    }
    assert_eq!(
        f.compactor.calls.load(Ordering::SeqCst),
        1,
        "continuation must not re-ingest the directive"
    );
    f.handle.stop().await.unwrap();
}

#[tokio::test]
async fn stop_during_dynamic_ingest_joins_compactor_and_restores_context() {
    let f = Fixture::new().await;
    let input = f.enter_ingest().await;
    tokio::time::timeout(Duration::from_secs(2), f.handle.stop())
        .await
        .expect("stop must not wait for episode compaction")
        .unwrap();
    assert!(matches!(input.await.unwrap(), Err(AgentError::Cancelled)));
    assert!(f.compactor.aborted.load(Ordering::SeqCst));
    assert_eq!(f.engine.checkpoint().await.unwrap(), f.before);
    assert_eq!(f.model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn completed_dynamic_ingest_admits_once_and_reports_compaction_once() {
    let mut f = Fixture::new().await;
    let input = f.enter_ingest().await;
    assert_eq!(f.model.requests.lock().unwrap().len(), 1);
    f.compactor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), input)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let events = completed(&mut f.events).await;
    assert!(!f.compactor.aborted.load(Ordering::SeqCst));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                RuntimeEvent::UserMessageAccepted { input }
                    if input.lifecycle == InputLifecycle::Applied && input.preview == NEW_INPUT
            ))
            .count(),
        1
    );
    let compactions: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ContextCompacted {
                reason,
                input_tokens,
                output_tokens,
                source_items,
                ..
            } => Some((*reason, *input_tokens, *output_tokens, *source_items)),
            _ => None,
        })
        .collect();
    assert_eq!(compactions.len(), 1);
    assert_eq!(compactions[0].0, CompactionReason::EpisodeRotation);
    assert_eq!((compactions[0].1, compactions[0].2), (17, 5));
    assert!(compactions[0].3 > 0);
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ContextMaintained {
            trigger: ContextMaintenanceTrigger::UserInput,
            report,
        } if report.compaction_input_tokens == 17 && report.compaction_output_tokens == 5
    )));

    f.handle.continue_active_task().await.unwrap();
    let events = completed(&mut f.events).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ContextCompacted { .. }))
    );
    assert_eq!(f.compactor.calls.load(Ordering::SeqCst), 1);
    {
        let requests = f.model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            assert!(request.messages.iter().any(|m| m.content == NEW_INPUT));
        }
    }
    f.handle.stop().await.unwrap();
}

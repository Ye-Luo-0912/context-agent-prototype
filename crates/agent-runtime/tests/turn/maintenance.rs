//! W04: before-model maintenance runs as a spawned, cancellable operation.
//! A blocked maintenance pass must not keep the actor from answering
//! cancel_turn, and aborting the pass must be the engine's safe failure.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport,
    RuntimeEvent, ScopeId, ScopeKind,
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
}

struct AbortGuard(Arc<AtomicBool>);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ContextEngine for GatedMaintenanceEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        if trigger == ContextMaintenanceTrigger::BeforeModel {
            let _guard = AbortGuard(self.aborted.clone());
            self.entered.notify_one();
            self.release.notified().await;
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
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
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
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let aborted = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(GatedMaintenanceEngine {
        entered: entered.clone(),
        release: release.clone(),
        aborted: aborted.clone(),
    });
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
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !aborted.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the gated maintenance future was not aborted");
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

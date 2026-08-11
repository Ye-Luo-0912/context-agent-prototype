//! Trusted authority primitives inside the stateless kernel (MOD-04 first
//! slice).
//!
//! The four authority seams — events, approval, effects, output — each get
//! one named home behind the existing `AgentKernel` facade. This slice only
//! *centralizes calls*; it is not yet proof that opaque effects are safe.
//! The point is the seam: every event gets its identity and durability
//! barrier here, every approval decision is a verdict here, every staged
//! effect commits or rolls back here, and every producer output passes the
//! broker here. A later Trusted Core implementation can replace this seam
//! without rewriting the kernel facade or the runtime actor.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_contracts::{
    AgentResult, ApprovalDecision, ApprovalGate, CancellationToken, Effect, EffectCommitError,
    EventJournal, IntentShadowGate, OutputBroker, RunId, RuntimeEvent, RuntimeEventEnvelope,
    ShadowVerdict, ToolCall, ToolOutput, ToolSpec,
};
use tokio::sync::broadcast;

/// Event identity, journaling and broadcast. The single write path for
/// runtime events: an envelope's run id, sequence and timestamp are minted
/// here, the journal append happens here, and the durability barrier
/// (`emit_durable`) is enforced here.
pub struct EventAuthority {
    journal: Option<Arc<dyn EventJournal>>,
    event_tx: broadcast::Sender<RuntimeEventEnvelope>,
    seq: Arc<AtomicU64>,
}

impl EventAuthority {
    pub fn new(
        journal: Option<Arc<dyn EventJournal>>,
        event_tx: broadcast::Sender<RuntimeEventEnvelope>,
        seq: Arc<AtomicU64>,
    ) -> Self {
        Self {
            journal,
            event_tx,
            seq,
        }
    }

    /// A live subscriber (the TUI, the eval harness). The broadcast sender
    /// is unbounded; lagging receivers drop old envelopes.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.event_tx.subscribe()
    }

    /// The broadcast sender behind `subscribe`, for live event sinks.
    pub fn sender(&self) -> broadcast::Sender<RuntimeEventEnvelope> {
        self.event_tx.clone()
    }

    /// The shared sequence counter, so live deltas and journaled events
    /// keep one consistent envelope order.
    pub fn seq(&self) -> Arc<AtomicU64> {
        self.seq.clone()
    }

    /// Mint one envelope: the next monotonic sequence for this run and a
    /// wall-clock timestamp. Sequencing is relaxed (ordering is enforced by
    /// the caller's await order, not by the counter itself).
    pub fn envelope(&self, run_id: RunId, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
            timestamp_ms: now_ms(),
            event,
        }
    }

    /// Journal + broadcast one runtime event. A journal failure is surfaced
    /// (the caller fences the turn); a broadcast with no listeners is not.
    pub async fn emit(&self, run_id: RunId, event: RuntimeEvent) -> AgentResult<()> {
        let envelope = self.envelope(run_id, event);
        if let Some(journal) = &self.journal {
            journal.append(&envelope).await?;
        }
        let _ = self.event_tx.send(envelope);
        Ok(())
    }

    /// Journal + broadcast one runtime event *after a durability barrier*:
    /// the event is appended, then `flush()` guarantees every event
    /// appended before it (the channel is FIFO) has left the process before
    /// the event is broadcast. Used at the turn-commit boundary: a
    /// subscriber never sees `TurnCompleted` unless the mandatory state
    /// writes before it are durable. A failed barrier returns the error and
    /// broadcasts nothing — the caller fences the turn instead of claiming
    /// a commit that never landed.
    pub async fn emit_durable(&self, run_id: RunId, event: RuntimeEvent) -> AgentResult<()> {
        let envelope = self.envelope(run_id, event);
        if let Some(journal) = &self.journal {
            journal.append(&envelope).await?;
            journal.flush().await?;
        }
        let _ = self.event_tx.send(envelope);
        Ok(())
    }

    /// Surface a runtime-level warning through the normal event stream.
    pub async fn warning(&self, run_id: RunId, message: String) -> AgentResult<()> {
        self.emit(run_id, RuntimeEvent::Warning { message }).await
    }

    /// Flush the journal (the durability barrier). The kernel calls this on
    /// stop; the actor never holds the flush on the turn hot path.
    pub async fn flush(&self) -> AgentResult<()> {
        if let Some(journal) = &self.journal {
            journal.flush().await?;
        }
        Ok(())
    }
}

/// The approval gate wrapped as a verdict. The gate decides; this authority
/// normalizes the three outcomes (allowed / denied / machinery failed) so
/// callers match a verdict instead of re-interpreting `AgentResult` + a
/// boolean, and so a future Core can substitute its own policy evaluation
/// behind the same shape. When a shadow gate is configured, the v2
/// intent-derived verdict is computed beside the legacy decision (never
/// enforced) so the invariant trace can be audited.
pub struct ApprovalAuthority {
    approval: Arc<dyn ApprovalGate>,
    shadow: Option<Arc<dyn IntentShadowGate>>,
}

/// The normalized outcome of one approval check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// The call may proceed.
    Allowed,
    /// The policy denied the call; the message is model-facing.
    Denied(String),
    /// The approval machinery itself failed; the message is model-facing.
    Failed(String),
}

impl ApprovalAuthority {
    pub fn new(approval: Arc<dyn ApprovalGate>) -> Self {
        Self {
            approval,
            shadow: None,
        }
    }

    /// Attach the v2 shadow gate (ACI v2 compatibility order step 4). The
    /// shadow verdict is recorded beside the legacy decision, never
    /// enforced.
    pub fn with_shadow(mut self, shadow: Arc<dyn IntentShadowGate>) -> Self {
        self.shadow = Some(shadow);
        self
    }

    /// Ask the gate and normalize its answer. Deny and error both produce a
    /// model-facing refusal; only `Allowed` lets the call reach the
    /// dispatcher.
    pub async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> ApprovalVerdict {
        match self.approval.authorize(call, spec, cancel).await {
            Ok(ApprovalDecision::Allow) => ApprovalVerdict::Allowed,
            Ok(ApprovalDecision::Deny) => {
                ApprovalVerdict::Denied(format!("tool denied by approval policy: {}", call.name))
            }
            Err(error) => ApprovalVerdict::Failed(format!("approval check failed: {error}")),
        }
    }

    /// Whether a shadow gate is attached (so the caller only publishes
    /// `ShadowDecision` events when there is a comparison to record).
    pub fn has_shadow(&self) -> bool {
        self.shadow.is_some()
    }

    /// The v2 shadow verdict for one call, when a shadow gate is attached.
    pub async fn shadow_verdict(&self, call: &ToolCall, spec: &ToolSpec) -> Option<ShadowVerdict> {
        if let Some(shadow) = &self.shadow {
            Some(shadow.shadow_verdict(call, spec).await)
        } else {
            None
        }
    }
}

/// Effect commit/rollback. The actor decides live-vs-stale (the generation
/// fence); this authority executes the mutation and classifies the outcome
/// (`NotApplied` leaves the world unchanged, `AppliedButDurabilityFailed`
/// means the effect landed but its record did not). Every staged effect of
/// every path — builtin, capability, wire broker — commits through this one
/// seam.
pub struct EffectAuthority;

impl EffectAuthority {
    /// Commit a staged effect. The `EffectCommitError` classification is
    /// preserved so the caller can tell the model the truth about what
    /// happened.
    pub async fn commit(&self, effect: Box<dyn Effect>) -> Result<(), EffectCommitError> {
        effect.commit().await
    }

    /// Roll a staged effect back because its operation turned stale or the
    /// turn aborted. The reason is surfaced through the effect's own
    /// bookkeeping.
    pub async fn rollback(&self, effect: Box<dyn Effect>, reason: &str) {
        effect.rollback(reason).await;
    }
}

/// The output broker as an authority: the only path from producer output to
/// a model-facing `ToolOutput`. Absent a broker, output passes through
/// unchanged and the runtime's last-line guard remains the backstop.
pub struct OutputAuthority {
    broker: Option<Arc<dyn OutputBroker>>,
}

impl OutputAuthority {
    pub fn new(broker: Option<Arc<dyn OutputBroker>>) -> Self {
        Self { broker }
    }

    /// Bound one producer output: caps every model-facing field and spills
    /// oversized content to an artifact. `budget` is the executed tool's
    /// declared per-tool output budget (`None` when no tool spec applies,
    /// e.g. an engine query result).
    pub async fn bound(
        &self,
        run_id: RunId,
        budget: Option<usize>,
        output: ToolOutput,
    ) -> ToolOutput {
        if let Some(broker) = &self.broker {
            broker.bound(run_id, budget, output).await
        } else {
            output
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AgentError, AgentResult, EventJournal, RuntimeEvent, RuntimeEventEnvelope, ToolCall,
        ToolRisk,
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };
    use tokio::sync::broadcast;

    // --- recording collaborators ---

    #[derive(Default)]
    struct RecordingJournal {
        appended: std::sync::Mutex<Vec<RuntimeEvent>>,
        flushed: AtomicUsize,
        fail_append: bool,
    }

    impl RecordingJournal {
        fn failing() -> Self {
            Self {
                fail_append: true,
                ..Self::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl EventJournal for RecordingJournal {
        async fn append(&self, envelope: &RuntimeEventEnvelope) -> AgentResult<()> {
            if self.fail_append {
                return Err(AgentError::Internal("journal append failed".into()));
            }
            self.appended.lock().unwrap().push(envelope.event.clone());
            Ok(())
        }
        async fn flush(&self) -> AgentResult<()> {
            self.flushed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct DenyGate(ApprovalDecision);

    #[async_trait::async_trait]
    impl ApprovalGate for DenyGate {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            Ok(self.0)
        }
    }

    struct FailingGate;

    #[async_trait::async_trait]
    impl ApprovalGate for FailingGate {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            Err(AgentError::Internal("gate exploded".into()))
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        calls: AtomicUsize,
        last_budget: std::sync::Mutex<Option<Option<usize>>>,
    }

    #[async_trait::async_trait]
    impl OutputBroker for RecordingBroker {
        async fn bound(
            &self,
            _run_id: RunId,
            budget: Option<usize>,
            output: ToolOutput,
        ) -> ToolOutput {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_budget.lock().unwrap() = Some(budget);
            output
        }
    }

    /// A staged effect that records whether it was committed or rolled back.
    struct RecordingEffect {
        action: Arc<std::sync::Mutex<&'static str>>,
        commit_fails: bool,
    }

    #[async_trait::async_trait]
    impl Effect for RecordingEffect {
        fn describe(&self) -> String {
            "recording effect".into()
        }
        async fn commit(self: Box<Self>) -> Result<(), EffectCommitError> {
            if self.commit_fails {
                return Err(EffectCommitError::NotApplied(AgentError::Internal(
                    "not applied".into(),
                )));
            }
            *self.action.lock().unwrap() = "committed";
            Ok(())
        }
        async fn rollback(self: Box<Self>, reason: &str) {
            *self.action.lock().unwrap() =
                Box::leak(format!("rolled back: {reason}").into_boxed_str());
        }
    }

    fn run() -> RunId {
        RunId::new()
    }

    fn output() -> ToolOutput {
        ToolOutput {
            call_id: "1".into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: "read".into(),
            model_content: "content".into(),
            artifact_ref: None,
            metadata: json!({}),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "test tool".into(),
            input_schema: json!({}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    // --- EventAuthority ---

    #[tokio::test]
    async fn event_authority_mints_monotonic_sequences_and_journals() {
        let journal = Arc::new(RecordingJournal::default());
        let (tx, _) = broadcast::channel(16);
        let seq = Arc::new(AtomicU64::new(0));
        let authority = EventAuthority::new(Some(journal.clone()), tx, seq);
        let run = run();

        authority.emit(run, RuntimeEvent::RunStarted).await.unwrap();
        authority
            .emit(run, RuntimeEvent::TurnCompleted)
            .await
            .unwrap();

        let appended = journal.appended.lock().unwrap();
        assert_eq!(appended.len(), 2);
        assert!(matches!(appended[0], RuntimeEvent::RunStarted));
        assert!(matches!(appended[1], RuntimeEvent::TurnCompleted));
        assert_eq!(authority.seq().load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn event_authority_durable_barrier_broadcasts_after_flush() {
        let journal = Arc::new(RecordingJournal::default());
        let (tx, mut rx) = broadcast::channel(16);
        let seq = Arc::new(AtomicU64::new(0));
        let authority = EventAuthority::new(Some(journal.clone()), tx, seq);
        let run = run();

        authority
            .emit_durable(run, RuntimeEvent::TurnCompleted)
            .await
            .unwrap();
        assert_eq!(journal.flushed.load(Ordering::SeqCst), 1);
        let envelope = rx.recv().await.expect("broadcast delivered");
        assert_eq!(envelope.run_id, run);
        assert_eq!(envelope.seq, 1);
        assert!(matches!(envelope.event, RuntimeEvent::TurnCompleted));
    }

    #[tokio::test]
    async fn event_authority_failed_barrier_broadcasts_nothing() {
        let journal = Arc::new(RecordingJournal::failing());
        let (tx, mut rx) = broadcast::channel(16);
        let seq = Arc::new(AtomicU64::new(0));
        let authority = EventAuthority::new(Some(journal), tx, seq);

        let result = authority
            .emit_durable(run(), RuntimeEvent::TurnCompleted)
            .await;
        assert!(result.is_err(), "a failed barrier must surface the error");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv(),)
                .await
                .is_err(),
            "nothing may be broadcast after a failed barrier"
        );
    }

    // --- ApprovalAuthority ---

    #[tokio::test]
    async fn approval_verdict_normalizes_allow_deny_failure() {
        let allow = ApprovalAuthority::new(Arc::new(DenyGate(ApprovalDecision::Allow)));
        let deny = ApprovalAuthority::new(Arc::new(DenyGate(ApprovalDecision::Deny)));
        let failed = ApprovalAuthority::new(Arc::new(FailingGate));

        let cancel = CancellationToken::new();
        assert_eq!(
            allow
                .authorize(&call("fs.read"), &spec("fs.read"), &cancel)
                .await,
            ApprovalVerdict::Allowed
        );
        assert_eq!(
            deny.authorize(&call("fs.write"), &spec("fs.write"), &cancel)
                .await,
            ApprovalVerdict::Denied("tool denied by approval policy: fs.write".to_string())
        );
        assert!(matches!(
            failed
                .authorize(&call("fs.write"), &spec("fs.write"), &cancel)
                .await,
            ApprovalVerdict::Failed(message) if message.contains("approval check failed")
        ));
    }

    // --- EffectAuthority ---

    #[tokio::test]
    async fn effect_authority_commits_and_rolls_back_through_one_seam() {
        let authority = EffectAuthority;

        let action = Arc::new(std::sync::Mutex::new("pending"));
        authority
            .commit(Box::new(RecordingEffect {
                action: action.clone(),
                commit_fails: false,
            }))
            .await
            .unwrap();
        assert_eq!(*action.lock().unwrap(), "committed");

        let action = Arc::new(std::sync::Mutex::new("pending"));
        authority
            .rollback(
                Box::new(RecordingEffect {
                    action: action.clone(),
                    commit_fails: false,
                }),
                "stale operation",
            )
            .await;
        assert!(
            action
                .lock()
                .unwrap()
                .starts_with("rolled back: stale operation"),
            "rollback must carry the reason"
        );

        // The commit-failure classification survives the seam.
        let action = Arc::new(std::sync::Mutex::new("pending"));
        let result = authority
            .commit(Box::new(RecordingEffect {
                action,
                commit_fails: true,
            }))
            .await;
        assert!(matches!(result, Err(EffectCommitError::NotApplied(_))));
    }

    // --- OutputAuthority ---

    #[tokio::test]
    async fn output_authority_bounds_with_broker_and_passes_through_without() {
        let broker = Arc::new(RecordingBroker::default());
        let bounded = OutputAuthority::new(Some(broker.clone()));
        let run = run();
        let result = bounded.bound(run, Some(512), output()).await;
        assert_eq!(result.tool_name, "fs.read");
        assert_eq!(broker.calls.load(Ordering::SeqCst), 1);
        assert_eq!(*broker.last_budget.lock().unwrap(), Some(Some(512)));

        let passthrough = OutputAuthority::new(None);
        let result = passthrough.bound(run, None, output()).await;
        assert_eq!(result.summary, "read");
    }
}

//! W4 (V7) acceptance: the full Compose -> Retry -> Cancel chain keeps the
//! usage a failed attempt already reported while the execution state stays
//! cancelled. The production retry assembly (`RetryingTransport` +
//! `JsonlRetryObserver::from_env`) is used verbatim; the optional metrics
//! artifact env var is absent, so the JSONL observer stays a no-op and the
//! settlement has to ride the formal account (the `ModelUsed` row), not the
//! diagnostic file.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_compose::{ComposeConfig, compose};
use agent_contracts::{
    AgentError, AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport,
    ModelUsage, RuntimeEvent, UsageIdentity,
};
use agent_core::PolicyApprovalGate;
use agent_workspace::Workspace;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use provider_openai::{JsonlRetryObserver, RetryingTransport};
use tokio::sync::broadcast::Receiver;

/// The provider shape of the V7 counterexample: the FIRST attempt fails with
/// a retryable transport error that already carries a usage report, and no
/// further attempt is ever issued (the test cancels during the backoff).
struct UsageThenBackoff {
    calls: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl ModelTransport for UsageThenBackoff {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(call, 1, "no retry may run after the backoff cancel");
        Err(AgentError::failed_with_usage(
            ModelUsage {
                input_tokens: Some(90),
                output_tokens: Some(30),
                ..ModelUsage::default()
            },
            AgentError::Transport {
                retryable: true,
                message: "stream dropped after the usage frame".into(),
            },
        ))
    }
}

/// Collect the ModelUsed rows and whether the turn was cancelled from the
/// event stream once the cancellation barrier has passed.
async fn drain_after_cancel(
    events: &mut Receiver<agent_contracts::RuntimeEventEnvelope>,
) -> (Vec<(u64, u64, UsageIdentity)>, bool) {
    let mut rows = Vec::new();
    let mut cancelled = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                    usage_identity,
                    ..
                } => rows.push((input_tokens, output_tokens, usage_identity)),
                RuntimeEvent::TurnCancelled { .. } => cancelled = true,
                _ => {}
            }
        }
        if cancelled && !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (rows, cancelled)
}

/// V7 primary counterexample: first attempt reports usage on a retryable
/// failure -> the user cancels during the backoff wait -> the optional
/// metrics file env var does not exist. The known usage must reach the
/// formal account (an observed `ModelUsed` row) while the execution state
/// stays cancelled, and the unexecuted retry must not add anything.
#[tokio::test]
async fn cancel_during_backoff_keeps_known_usage_and_cancelled_state() {
    // The optional JSONL artifact must not exist for this chain: the formal
    // settlement cannot depend on a diagnostic env var. No test in this
    // binary sets it; assert that instead of mutating process state.
    assert!(std::env::var("OPENAI_RETRY_METRICS_FILE").is_err());

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let workspace = Workspace::open(&root).await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    // The production retry assembly from `provider_from_env_with_timeout`:
    // same wrapper, same `JsonlRetryObserver::from_env()` observer wiring,
    // with a base delay long enough that the cancel lands inside the wait.
    let model: Arc<dyn ModelTransport> = Arc::new(
        RetryingTransport::new(
            UsageThenBackoff {
                calls: calls.clone(),
            },
            3,
            Duration::from_secs(60),
        )
        .with_observer(Arc::new(JsonlRetryObserver::from_env())),
    );
    let config = ComposeConfig {
        provider_profile_digest: None,
        cache_routing: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        model,
        approval: Arc::new(PolicyApprovalGate::permissive()),
        base_tools: Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone()).unwrap()),
        capability_aware: false,
        journal: None,
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: None,
        effect_reservation_journal: None,
        verification_recipes: None,
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    };
    let composed = compose(config).await.unwrap();
    composed.instance.start().await.unwrap();
    let handle = composed.handle().clone();
    let mut events = composed.subscribe();

    handle.user_message("hello".into()).await.unwrap();
    // The first attempt fails immediately; the retry loop is now parked in
    // the 60s backoff. Cancel from the operator side, as a user would.
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.cancel_turn().await.unwrap();

    let (rows, cancelled) = drain_after_cancel(&mut events).await;
    assert!(cancelled, "the turn must still end cancelled");
    let observed: Vec<(u64, u64, UsageIdentity)> = rows
        .iter()
        .filter(|(_, _, identity)| *identity == UsageIdentity::Observed)
        .copied()
        .collect();
    assert_eq!(
        observed,
        vec![(90, 30, UsageIdentity::Observed)],
        "the attempt's reported counters must survive the cancellation: got {rows:?}"
    );
    // The cancel barrier's unknown row stays honest (zeros, unknown
    // identity) — it supplements, never replaces, and never invents.
    let unknown: Vec<(u64, u64, UsageIdentity)> = rows
        .iter()
        .filter(|(_, _, identity)| *identity == UsageIdentity::Unknown)
        .copied()
        .collect();
    assert_eq!(unknown, vec![(0, 0, UsageIdentity::Unknown)]);
    // The unexecuted retry is not counted as an executed attempt.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The diagnostic artifact never existed: the observer had no path.
    assert!(std::env::var("OPENAI_RETRY_METRICS_FILE").is_err());

    composed.shutdown().await.unwrap();
}

//! R2 acceptance: the usage a provider already reported must reach the formal
//! account whichever terminal shape the late result has.
//!
//! W4 covered one shape — a retry backoff cancelled mid-wait, where the
//! transport returns `FailedWithUsage { source: Cancelled }` and the outcome
//! becomes `Cancelled { known_usage }`. The continuation review found the
//! completion matrix was still incomplete: after the cancel barrier had
//! written its unknown placeholder and marked the operation accounted, a late
//! `ModelOutput { usage }` or `Failed { usage }` matched no settlement branch,
//! so the business result was correctly discarded *together with* its known
//! cost.
//!
//! These two tests park the provider call until the operator's cancel has been
//! handled, then let it finish with real counters. That is the legal race the
//! review describes: a cooperative provider that still completes after the
//! cancellation. No vendor has to ignore cancellation for this to happen.

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
use tokio::sync::{Notify, broadcast::Receiver};

/// A provider whose first call parks until the test releases it, then returns
/// the terminal shape the case is about. Later calls are a test failure: a
/// superseded operation must never be retried.
struct ParkedOutcome {
    calls: Arc<AtomicU32>,
    release: Arc<Notify>,
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The round actually succeeded and reported counters.
    Success,
    /// The round failed and the typed error carried counters.
    Failure,
}

#[async_trait::async_trait]
impl ModelTransport for ParkedOutcome {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(call, 1, "a superseded operation must not be retried");
        self.release.notified().await;
        let usage = ModelUsage {
            input_tokens: Some(70),
            output_tokens: Some(11),
            cached_input_tokens: Some(5),
            ..ModelUsage::default()
        };
        match self.mode {
            Mode::Success => Ok(ModelOutput {
                content: "late but real".into(),
                tool_calls: Vec::new(),
                usage,
            }),
            Mode::Failure => Err(AgentError::failed_with_usage(
                usage,
                AgentError::Transport {
                    retryable: false,
                    message: "provider reported counters before failing".into(),
                },
            )),
        }
    }
}

/// The formal account rows, the cancellation state, and whether any tool ran.
struct Observed {
    rows: Vec<(u64, u64, u64, UsageIdentity)>,
    cancelled: bool,
    tools_started: usize,
}

async fn drain(events: &mut Receiver<agent_contracts::RuntimeEventEnvelope>) -> Observed {
    let mut observed = Observed {
        rows: Vec::new(),
        cancelled: false,
        tools_started: 0,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    usage_identity,
                    ..
                } => observed
                    .rows
                    .push((input_tokens, output_tokens, cached_input_tokens, usage_identity)),
                RuntimeEvent::TurnCancelled { .. } => observed.cancelled = true,
                RuntimeEvent::ToolStarted { .. } => observed.tools_started += 1,
                _ => {}
            }
        }
        if observed.cancelled && !observed.rows.is_empty() {
            // Give any second settlement a chance to (wrongly) appear.
            tokio::time::sleep(Duration::from_millis(300)).await;
            while let Ok(envelope) = events.try_recv() {
                if let RuntimeEvent::ModelUsed {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    usage_identity,
                    ..
                } = envelope.event
                {
                    observed
                        .rows
                        .push((input_tokens, output_tokens, cached_input_tokens, usage_identity));
                }
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    observed
}

async fn run_case(mode: Mode) -> Observed {
    assert!(std::env::var("OPENAI_RETRY_METRICS_FILE").is_err());
    let temp = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(temp.path()).await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let release = Arc::new(Notify::new());
    let model: Arc<dyn ModelTransport> = Arc::new(ParkedOutcome {
        calls: calls.clone(),
        release: release.clone(),
        mode,
    });
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
    // Wait until the provider call is actually parked, then cancel: the
    // barrier and its unknown placeholder land BEFORE the result does.
    let start = tokio::time::Instant::now();
    while calls.load(Ordering::SeqCst) == 0 {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the provider call never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle.cancel_turn().await.unwrap();
    // Now let the parked round finish with its real counters.
    release.notify_waiters();

    let observed = drain(&mut events).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the superseded operation must not run again"
    );
    composed.shutdown().await.unwrap();
    observed
}

/// A late SUCCESSFUL round: its content must not be adopted, no tool may run,
/// and its reported counters must still be booked exactly once.
#[tokio::test]
async fn a_late_successful_round_still_settles_its_known_usage_once() {
    let observed = run_case(Mode::Success).await;
    assert!(observed.cancelled, "the turn must stay cancelled");
    assert_eq!(
        observed.tools_started, 0,
        "a superseded round must not execute the tools it returned"
    );
    let booked: Vec<_> = observed
        .rows
        .iter()
        .filter(|(_, _, _, identity)| *identity == UsageIdentity::Observed)
        .copied()
        .collect();
    assert_eq!(
        booked,
        vec![(70, 11, 5, UsageIdentity::Observed)],
        "the late round's counters must reach the account exactly once: {:?}",
        observed.rows
    );
}

/// A late FAILED round carrying counters: same accounting, no restart.
#[tokio::test]
async fn a_late_failed_round_still_settles_its_known_usage_once() {
    let observed = run_case(Mode::Failure).await;
    assert!(observed.cancelled, "the turn must stay cancelled");
    let booked: Vec<_> = observed
        .rows
        .iter()
        .filter(|(_, _, _, identity)| *identity == UsageIdentity::Observed)
        .copied()
        .collect();
    assert_eq!(
        booked,
        vec![(70, 11, 5, UsageIdentity::Observed)],
        "the late failure's counters must reach the account exactly once: {:?}",
        observed.rows
    );
}

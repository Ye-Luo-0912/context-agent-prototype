//! T7: ONE continuous same-task backend development journey over the real
//! host wire — not several unrelated green rows stitched together.
//!
//! One TaskId, one workspace, one restore lineage:
//!
//!   submit a real cross-file goal -> the scripted model drives the Runtime
//!   through REAL tool calls (fs.write / fs.read land real workspace files,
//!   every write passes the interactive approval gate over the wire) -> a
//!   mid-turn user correction (work.steer, `Queued` into the running turn's
//!   single correction slot) is incorporated into the execution -> the turn
//!   continuation is cancelled mid-flight (durable `Cancelled` barrier) -> a
//!   formal checkpoint lands in the run's own store -> the whole host side
//!   shuts down -> a NEW composition + NEW host process (same workspace dir)
//!   cold-restores the SAME task over the wire -> the external changes are
//!   still on disk -> the remaining goal work completes through real tools
//!   again -> delivery checks (task still `Active` under the default
//!   `OperatorClosureOnly` policy — an ordinary final is NOT a durable
//!   completion) -> the local operator surface closes the task explicitly,
//!   and the typed `TaskCompleted` event carries the operator summary plus
//!   the final output's digest.
//!
//! A second test runs the key failure variant: after the cold restore, one
//! transient real tool failure (a write whose parent directory does not
//! exist yet) must not lose the task state; resolving the external
//! condition and continuing again completes the very same write.
//!
//! Everything runs on the shared C0 wire shapes (named pipe on Windows, UDS
//! on unix — the long-flow dual-endpoint pattern) with a scripted model; no
//! vendor is called. All other components are the real product pieces:
//! RuntimeActor, Core approval/effect path, checkpoint plane, tool-runtime.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use agent_compose::{
    ComposeConfig, ComposedRuntime, ContextPolicy, HostToolPolicyRegistry, MaintenanceBudget,
    build_context_engine, compose,
};
use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ModelCapabilities, ModelOutput, ModelRequest,
    ModelRole, ModelTransport, RuntimeEvent, ToolCall,
};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
use agent_host::{
    HostPlane, HostServer, LocalEndpoint, SingleInstance, negotiated_profile, session_schema_digest,
};
use agent_platform_protocol::{
    ActiveFeatures, ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse,
    Causality, EnvelopeKind, MessageId, PlatformEnvelope, PlatformResponse, ProtocolIdentity,
    ProtocolVersion, RequestId, Route, TaskSnapshotStatus, WorkCancelRequest, WorkCancelResponse,
    WorkCheckpointRequest, WorkCheckpointResponse, WorkContinueDisposition, WorkContinueReason,
    WorkContinueRequest, WorkContinueResponse, WorkRestoreRequest, WorkRestoreResponse,
    WorkSnapshotRequest, WorkSnapshotResponse, WorkSteerDisposition, WorkSteerRequest,
    WorkSteerResponse, WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse,
    WorkTaskCompletionRequest, WorkTaskCompletionResponse, WorkTaskDetailRequest,
    WorkTaskDetailResponse,
};
use agent_runtime::{RuntimeHandle, WorkControlSessionRegistry};
use agent_workspace::Workspace;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::sync::watch;

// ---------------------------------------------------------------------------
// The scripted journey's fixed facts.
// ---------------------------------------------------------------------------

/// The submitted goal. It names ALL of the journey's work up front, so the
/// post-restore continuation is genuinely "the remaining work of the same
/// task", never a second task.
const GOAL: &str = "t7 journey: create part_a.md and part_b.md in the workspace, \
                    then a summary.md at the workspace root that references part_b's line";

/// The mid-task user correction. It CHANGES the output: part_b must prefix
/// the quoted headline with `Q: `.
const STEER: &str = "t7 correction: when you write part_b.md, prefix the quoted \
                     headline line with \"Q: \" exactly";

const PART_A: &str = "headline: the parser owns tokenization";
const FINAL_A: &str = "[t7] part A staged; holding for the user's quote correction.";
const FINAL_B: &str = "[t7] part B quotes the corrected headline.";
const FINAL_HELD: &str = "[t7] continuation held; the operator interrupted this turn.";
const FINAL_SUMMARY: &str = "[t7] summary.md references part B; all goal work is complete.";
const OPERATOR_SUMMARY: &str =
    "t7 operator: verified part_a/part_b/summary contents on disk; closing the task.";

/// The failure variant's goal and scripted texts. `archive/summary.md`
/// deliberately targets a missing parent directory: the first post-restore
/// attempt fails as a REAL tool failure (`parent_path_not_found`), the
/// condition is then resolved externally, and the retry completes the same
/// write under the same task.
const FAILURE_GOAL: &str = "t7 failure variant: create part_a.md in the workspace and an \
                            archive/summary.md that summarizes its headline";
const FAILURE_PART_A: &str = "headline: the archiver reads what the parser wrote";
const FAILURE_FINAL_A: &str = "[t7-f] part A staged.";
const FAILURE_FINAL_BLOCKED: &str =
    "[t7-f] the archive write was refused (parent missing); will retry once the condition clears.";
const FAILURE_FINAL_SUMMARY: &str = "[t7-f] archive/summary.md written after the retry.";

// ---------------------------------------------------------------------------
// Scripted models. The step counter continues across the process restart
// (constructor start offset), exactly like the established scripted-model
// harnesses; each round also reacts to real tool results where the journey
// needs the file's true content.
// ---------------------------------------------------------------------------

/// A park the test releases: the scripted model holds the turn open so the
/// mid-turn steer and the mid-turn cancel are deterministic instead of
/// racing the turn tail. Fail-open when the sender is gone.
async fn wait_released(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

fn scripted_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tool_calls: true,
        max_output_tokens: 4096,
        context_window: None,
    }
}

fn text_output(content: &str) -> ModelOutput {
    ModelOutput {
        content: content.to_string(),
        tool_calls: Vec::new(),
        usage: Default::default(),
    }
}

fn write_output(path: &str, content: &str, serial: usize) -> ModelOutput {
    ModelOutput {
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: format!("t7-write-{serial}"),
            name: "fs.write".into(),
            arguments: json!({"path": path, "content": content}),
        }],
        usage: Default::default(),
    }
}

fn read_output(path: &str, serial: usize) -> ModelOutput {
    ModelOutput {
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: format!("t7-read-{serial}"),
            name: "fs.read".into(),
            arguments: json!({"path": path}),
        }],
        usage: Default::default(),
    }
}

/// Extracts the file line the last `fs.read` tool result carried. The
/// fs.read body renders selected lines as `     N | <line>`; taking the
/// text after ` | ` binds the model's next write to the REAL file content
/// that traveled through the tool result, not to a scripted copy.
fn quoted_line(request: &ModelRequest, which: &str) -> String {
    for message in request.messages.iter().rev() {
        if message.role != ModelRole::Tool {
            continue;
        }
        for line in message.content.lines() {
            if let Some(pos) = line.find(" | ") {
                return line[pos + 3..].trim().to_string();
            }
        }
    }
    eprintln!("[t7 model] no fs.read line found for {which}; using a sentinel");
    format!("__MISSING_{which}_LINE__")
}

/// The main journey's script. Round map:
///   0 write part_a.md             (session 1, from submit)
///   1 HOLD the turn open          (the test steers mid-turn, then releases)
///     final FINAL_A
///   2 read part_a.md              (turn 2 — the queued steer was applied)
///   3 write part_b.md = "Q: " + the REAL part_a line
///   4 final FINAL_B
///   5 HOLD the turn open again    (the test cancels mid-turn)
///     final FINAL_HELD
///   6 read part_b.md              (session 2, post cold restore, /continue)
///   7 write summary.md referencing the REAL part_b line
///   8 final FINAL_SUMMARY
struct JourneyModel {
    step: AtomicUsize,
    parks: std::sync::Mutex<Vec<watch::Receiver<bool>>>,
}

impl JourneyModel {
    fn new(start: usize, parks: Vec<watch::Receiver<bool>>) -> Arc<Self> {
        Arc::new(Self {
            step: AtomicUsize::new(start),
            parks: std::sync::Mutex::new(parks),
        })
    }

    fn take_park(&self, what: &str) -> watch::Receiver<bool> {
        // FIFO: the parks were armed in script order (the part A hold
        // before the cancel hold).
        let mut parks = self.parks.lock().expect("park queue mutex");
        if parks.is_empty() {
            panic!("no park gate left for {what}");
        }
        parks.remove(0)
    }
}

#[async_trait::async_trait]
impl ModelTransport for JourneyModel {
    fn capabilities(&self) -> ModelCapabilities {
        scripted_capabilities()
    }

    async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        eprintln!("[t7 journey model] scripted round {step}");
        match step {
            0 => Ok(write_output("part_a.md", PART_A, 0)),
            1 => {
                let rx = self.take_park("the part A hold");
                wait_released(rx).await;
                Ok(text_output(FINAL_A))
            }
            2 => Ok(read_output("part_a.md", 2)),
            3 => {
                let line = quoted_line(&request, "PART_A");
                Ok(write_output("part_b.md", &format!("Q: {line}"), 3))
            }
            4 => Ok(text_output(FINAL_B)),
            5 => {
                let rx = self.take_park("the cancel hold");
                tokio::select! {
                    _ = wait_released(rx) => {}
                    _ = request.cancel.cancelled() => {}
                }
                Ok(text_output(FINAL_HELD))
            }
            6 => Ok(read_output("part_b.md", 6)),
            7 => {
                let line = quoted_line(&request, "PART_B");
                Ok(write_output(
                    "summary.md",
                    &format!("summary of: {line}"),
                    7,
                ))
            }
            8 => Ok(text_output(FINAL_SUMMARY)),
            other => Err(AgentError::InvalidRequest(format!(
                "t7 journey model reached an unexpected scripted round {other}"
            ))),
        }
    }
}

/// The failure variant's script:
///   0 write part_a.md             (session 1)
///   1 final
///   2 write archive/summary.md    (session 2, post restore: REAL tool
///                                 failure — the parent directory is absent)
///   3 final FAILURE_FINAL_BLOCKED (the turn ends; the task state survives)
///   4 write archive/summary.md    (retry after the test resolved the
///                                 condition — the SAME write succeeds)
///   5 final
struct FailureModel {
    step: AtomicUsize,
}

impl FailureModel {
    fn new(start: usize) -> Arc<Self> {
        Arc::new(Self {
            step: AtomicUsize::new(start),
        })
    }
}

#[async_trait::async_trait]
impl ModelTransport for FailureModel {
    fn capabilities(&self) -> ModelCapabilities {
        scripted_capabilities()
    }

    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        eprintln!("[t7 failure model] scripted round {step}");
        match step {
            0 => Ok(write_output("part_a.md", FAILURE_PART_A, 0)),
            1 => Ok(text_output(FAILURE_FINAL_A)),
            2 => Ok(write_output("archive/summary.md", FAILURE_PART_A, 2)),
            3 => Ok(text_output(FAILURE_FINAL_BLOCKED)),
            4 => Ok(write_output("archive/summary.md", FAILURE_PART_A, 4)),
            5 => Ok(text_output(FAILURE_FINAL_SUMMARY)),
            other => Err(AgentError::InvalidRequest(format!(
                "t7 failure model reached an unexpected scripted round {other}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture: the product composition the host binary builds, with the
// durable surfaces the journey needs (event journal, artifact store, effect
// reservation journal) — the approval stack is the host shape: built-in tool
// policies over the interactive gate, so every workspace write parks until
// the client answers `approval/respond` ON THE WIRE.
// ---------------------------------------------------------------------------

struct Fixture {
    composed: ComposedRuntime,
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
    _lock: SingleInstance,
}

async fn journey_workspace(
    root: &std::path::Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<Fixture> {
    let workspace = Workspace::open(root).await?;
    let lock = SingleInstance::acquire(workspace.state_dir())?;
    // Rolling policy like the product host, but with NO maintenance model:
    // a scripted step machine must never receive an engine maintenance call.
    let context_engine = build_context_engine(
        ContextPolicy::Rolling,
        workspace.state_dir(),
        None,
        None,
        &MaintenanceBudget::default(),
        None,
    )
    .await?;
    let verification_recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&verification_recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
    let task_gate =
        Arc::new(TaskApprovalGate::new(gate.clone()).with_host_policies(host_policies.clone()));
    let base_tools = Arc::new(tool_runtime::BuiltinToolDispatcher::new(workspace.clone())?);
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        cache_routing: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(
            workspace
                .state_dir()
                .join("authority")
                .join("broker-reservations.jsonl"),
        ),
        verification_recipes: Some(verification_recipes),
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await?;
    Ok(Fixture {
        composed,
        broker,
        gate,
        _lock: lock,
    })
}

/// The host-owned run configuration, mirroring the binary (long-flow shape).
fn test_run_config() -> agent_runtime::HostRunConfig {
    agent_runtime::HostRunConfig {
        context_policy: "rolling".into(),
        max_model_rounds_from_cli: false,
        maintenance_max_calls_per_maintain: 4,
        maintenance_max_tokens_per_maintain: None,
        maintenance_timeout_secs: None,
        provider_profile_digest: None,
        prompt_cache_mode: None,
        read_only: false,
    }
}

// ---------------------------------------------------------------------------
// Wire helpers (the long-flow pattern from host_e2e).
// ---------------------------------------------------------------------------

fn client_protocol() -> ProtocolIdentity {
    ProtocolIdentity {
        name: "focus-agent.platform".into(),
        version: ProtocolVersion { major: 1, minor: 0 },
        active_features: ActiveFeatures::default(),
        schema_digest: session_schema_digest(),
    }
}

fn request<P>(namespace: &str, operation: &str, payload: P) -> PlatformEnvelope<P> {
    let message_id = MessageId::new();
    PlatformEnvelope {
        protocol: client_protocol(),
        message_id,
        request_id: Some(RequestId::new()),
        kind: EnvelopeKind::Request,
        route: Route {
            namespace: namespace.into(),
            operation: operation.into(),
        },
        work: None,
        causality: Causality::root(message_id),
        payload,
    }
}

fn expect_value<T>(response: PlatformResponse<T>) -> T {
    match response {
        PlatformResponse::Success { value } => value,
        PlatformResponse::Error { error } => panic!("expected success, got error {error:?}"),
    }
}

fn exchange<S: Read + Write, P: serde::Serialize, R: serde::de::DeserializeOwned>(
    stream: &mut S,
    request: &PlatformEnvelope<P>,
) -> anyhow::Result<PlatformResponse<R>> {
    agent_host::write_frame(stream, &serde_json::to_vec(request)?)?;
    let frame = agent_host::read_frame(stream)?
        .ok_or_else(|| anyhow::anyhow!("connection closed before response"))?;
    let envelope = serde_json::from_slice::<PlatformEnvelope<PlatformResponse<R>>>(&frame)?;
    assert_eq!(
        &envelope.request_id, &request.request_id,
        "correlated response"
    );
    assert_eq!(envelope.kind, EnvelopeKind::Response);
    Ok(envelope.payload)
}

#[cfg(windows)]
async fn connect(endpoint: &LocalEndpoint) -> std::fs::File {
    let LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    let path = format!(r"\\.\pipe\{name}");
    let started = std::time::Instant::now();
    while started.elapsed() < CONNECT_BUDGET {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            return file;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("named pipe {path} never became connectable");
}

#[cfg(unix)]
async fn connect(endpoint: &LocalEndpoint) -> std::os::unix::net::UnixStream {
    let LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    let started = std::time::Instant::now();
    while started.elapsed() < CONNECT_BUDGET {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(path) {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("uds socket {} never became connectable", path.display());
}

#[cfg(windows)]
fn try_connect_once(endpoint: &LocalEndpoint) -> bool {
    let LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!(r"\\.\pipe\{name}"))
        .is_ok()
}

#[cfg(unix)]
fn try_connect_once(endpoint: &LocalEndpoint) -> bool {
    let LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

const CONNECT_BUDGET: Duration = Duration::from_secs(30);
const BOUNDED_STOP: Duration = Duration::from_secs(20);

struct TestServer {
    endpoint: LocalEndpoint,
    stop: Arc<std::sync::atomic::AtomicBool>,
    serve: std::thread::JoinHandle<anyhow::Result<()>>,
}

async fn start_server(fixture: &Fixture, endpoint: LocalEndpoint) -> anyhow::Result<TestServer> {
    use std::sync::atomic::AtomicBool;

    let handle: RuntimeHandle = fixture.composed.handle().clone();
    let registry = WorkControlSessionRegistry::new(handle.run_id());
    let plane = HostPlane {
        profile: negotiated_profile()?,
        handle,
        broker: Arc::clone(&fixture.broker),
        gate: Arc::clone(&fixture.gate),
        registry,
        workspace: Arc::new(fixture.composed.workspace.clone()),
        checkpoints: Some(fixture.composed.instance.checkpoint_plane()),
        run_config: Some(test_run_config()),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: false,
        stop: Arc::clone(&stop),
    };
    let runtime = tokio::runtime::Handle::current();
    let serve = std::thread::spawn(move || server.serve(plane, runtime));
    let test_server = TestServer {
        endpoint,
        stop,
        serve,
    };
    // One throwaway connection proves the endpoint is bound; if the serve
    // loop died before binding, fail now with its actual error.
    let started = std::time::Instant::now();
    loop {
        if test_server.serve.is_finished() {
            let error = match test_server.serve.join() {
                Ok(Ok(())) => "exited cleanly without ever binding".to_string(),
                Ok(Err(error)) => format!("failed: {error:#}"),
                Err(_) => "panicked".to_string(),
            };
            anyhow::bail!("host serve loop died before binding: {error}");
        }
        if try_connect_once(&test_server.endpoint) {
            break;
        }
        anyhow::ensure!(
            started.elapsed() < CONNECT_BUDGET,
            "endpoint never became connectable"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(test_server)
}

async fn stop_and_join_bounded(server: TestServer) -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;

    server.stop.store(true, Ordering::SeqCst);
    let _ = connect(&server.endpoint).await;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(server.serve.join());
    });
    let joined = done_rx
        .recv_timeout(BOUNDED_STOP)
        .map_err(|_| anyhow::anyhow!("serve loop did not exit within {BOUNDED_STOP:?}"))?;
    joined.map_err(|_| anyhow::anyhow!("serve thread panicked"))??;
    Ok(())
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{serial}-{}", std::process::id())
}

// ---------------------------------------------------------------------------
// Journey helpers.
// ---------------------------------------------------------------------------

fn snapshot_request() -> PlatformEnvelope<WorkSnapshotRequest> {
    request("work", "snapshot", WorkSnapshotRequest {})
}

fn readiness_of(snapshot: &WorkSnapshotResponse) -> WorkContinueReason {
    snapshot
        .continue_readiness
        .as_ref()
        .map(|readiness| readiness.reason)
        .expect("the snapshot reports why continuation is or is not available")
}

/// Approves whatever is pending over the wire, then waits until the runtime
/// is idle. This is the GUI's real loop: the turn parks inside the
/// interactive approval gate until `approval/respond` delivers the decision.
async fn drive_turn_to_idle<S: Read + Write>(
    stream: &mut S,
    what: &str,
) -> anyhow::Result<WorkSnapshotResponse> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
            stream,
            &snapshot_request(),
        )?);
        if let Some(pending) = snapshot.pending_approvals.first() {
            let answered = expect_value(exchange::<_, _, ApprovalRespondResponse>(
                stream,
                &request(
                    "approval",
                    "respond",
                    ApprovalRespondRequest {
                        request_id: pending.request_id.clone(),
                        decision: ApprovalDecision::Allow,
                    },
                ),
            )?);
            assert_eq!(answered.outcome, ApprovalRespondOutcome::Delivered);
            continue;
        }
        if readiness_of(&snapshot) != WorkContinueReason::TurnRunning {
            return Ok(snapshot);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the runtime never left the running state while waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until the turn is RUNNING with nothing pending approval — twice in
/// a row — which means the scripted model holds the turn open in its parked
/// round. From this point the turn cannot end by itself, so a mid-turn
/// steer (Queued) and a mid-turn cancel are deterministic.
async fn wait_turn_parked<S: Read + Write>(stream: &mut S) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut quiet = 0usize;
    loop {
        let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
            stream,
            &snapshot_request(),
        )?);
        if let Some(pending) = snapshot.pending_approvals.first() {
            let answered = expect_value(exchange::<_, _, ApprovalRespondResponse>(
                stream,
                &request(
                    "approval",
                    "respond",
                    ApprovalRespondRequest {
                        request_id: pending.request_id.clone(),
                        decision: ApprovalDecision::Allow,
                    },
                ),
            )?);
            assert_eq!(answered.outcome, ApprovalRespondOutcome::Delivered);
            quiet = 0;
        } else if readiness_of(&snapshot) == WorkContinueReason::TurnRunning {
            quiet += 1;
            if quiet >= 2 {
                return Ok(());
            }
        } else {
            quiet = 0;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "the turn never reached its parked round"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_event(
    events: &mut broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    matches: impl Fn(&RuntimeEvent) -> bool,
    what: &str,
) -> RuntimeEvent {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(envelope) = events.try_recv()
            && matches(&envelope.event)
        {
            return envelope.event.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the runtime never {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Committed change rows in the workspace journal naming `needle` — the
/// durable effect record, independent of file content.
fn change_rows(root: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(root.join(".focus-agent").join("changes.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

// ---------------------------------------------------------------------------
// T7 main journey: one task, one workspace, one restore lineage.
// ---------------------------------------------------------------------------

async fn same_task_journey(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();

    // ============================ SESSION 1 ============================
    // The parks: gate 0 holds turn 1 for the mid-turn steer; gate 1 holds
    // the continued turn for the mid-turn cancel.
    let (steer_release, steer_hold) = watch::channel(false);
    let (cancel_release, cancel_hold) = watch::channel(false);
    // Survives the session: the task identity, the checkpoint artifact name
    // and session 1's run id (the lineage anchor).
    let task_id;
    let artifact_name;
    let run_id_1;
    {
        let model = JourneyModel::new(0, vec![steer_hold, cancel_hold]);
        let fixture = journey_workspace(&root, model).await?;
        let mut events = fixture.composed.subscribe();
        let handle: RuntimeHandle = fixture.composed.handle().clone();
        fixture.composed.instance.start().await?;
        run_id_1 = handle.run_id();
        let server = start_server(&fixture, endpoint.clone()).await?;
        let mut stream = connect(&server.endpoint).await;
        eprintln!("t7[session-1]: connected; run {}", run_id_1);

        // 1. SUBMIT: the cross-file goal creates and focuses ONE task.
        let submitted = expect_value(exchange::<_, _, WorkSubmitResponse>(
            &mut stream,
            &request(
                "work",
                "submit",
                WorkSubmitRequest {
                    goal: GOAL.into(),
                    client_request_id: "t7-journey-1".into(),
                },
            ),
        )?);
        assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
        task_id = submitted.task_id;
        eprintln!("t7[session-1]: submitted -> task {task_id}");

        // An idempotent retry of the same submission returns the SAME task —
        // the journey's identity does not fork.
        let retried = expect_value(exchange::<_, _, WorkSubmitResponse>(
            &mut stream,
            &request(
                "work",
                "submit",
                WorkSubmitRequest {
                    goal: GOAL.into(),
                    client_request_id: "t7-journey-1".into(),
                },
            ),
        )?);
        assert_eq!(retried.disposition, WorkSubmitDisposition::AlreadyAccepted);
        assert_eq!(retried.task_id, task_id);

        // 2. The turn runs with REAL tools. Round 0 emits fs.write part_a.md;
        //    the write parks in the interactive approval gate until this client
        //    answers on the wire; then the model reaches its parked round.
        wait_turn_parked(&mut stream).await?;
        assert!(
            root.join("part_a.md").exists(),
            "part_a.md must be a real workspace artifact before the correction"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("part_a.md"))?,
            PART_A,
            "the model's fs.write landed the real file"
        );
        assert!(!root.join("part_b.md").exists());

        // 3. MID-TURN CORRECTION: the steer rides the running turn's single
        //    correction slot (Queued), names its task, and creates no second
        //    task. The runtime drains it into a fresh turn when the held turn
        //    ends.
        let steered = expect_value(exchange::<_, _, WorkSteerResponse>(
            &mut stream,
            &request(
                "work",
                "steer",
                WorkSteerRequest {
                    instruction: STEER.into(),
                    expected_task_id: Some(task_id),
                },
            ),
        )?);
        assert_eq!(
            steered.disposition,
            WorkSteerDisposition::Queued,
            "the correction was admitted into the running turn's slot"
        );
        assert_eq!(steered.task_id, Some(task_id));
        eprintln!("t7[session-1]: steer queued into the held turn");

        steer_release
            .send(true)
            .map_err(|_| anyhow::anyhow!("steer release receiver gone"))?;
        // The held turn ends with its final; the queued correction is drained
        // into turn 2, which READS part_a.md for real and writes part_b.md with
        // the corrected `Q: ` prefix.
        let after_steer = drive_turn_to_idle(&mut stream, "the corrected turn").await?;
        assert_eq!(
            after_steer.tasks.len(),
            1,
            "steering must never create a second task"
        );
        let part_b = std::fs::read_to_string(root.join("part_b.md"))?;
        assert_eq!(
            part_b,
            format!("Q: {PART_A}"),
            "part_b.md must quote part_a.md's REAL headline (via fs.read) with the steered prefix"
        );
        eprintln!("t7[session-1]: correction incorporated -> part_b = {part_b:?}");

        // 4. INTERRUPTION: continue the task's retained directive, hold the new
        //    turn in its parked round, then cancel it mid-flight. `Cancelled`
        //    is Core's durable-barrier proof, and the ack names this task.
        let continued = expect_value(exchange::<_, _, WorkContinueResponse>(
            &mut stream,
            &request(
                "work",
                "continue",
                WorkContinueRequest {
                    expected_task_id: Some(task_id),
                },
            ),
        )?);
        assert_eq!(continued.disposition, WorkContinueDisposition::Continued);
        assert_eq!(continued.task_id, Some(task_id));
        wait_turn_parked(&mut stream).await?;

        let cancelled = expect_value(exchange::<_, _, WorkCancelResponse>(
            &mut stream,
            &request(
                "work",
                "cancel",
                WorkCancelRequest {
                    expected_task_id: Some(task_id),
                    expected_turn_id: None,
                },
            ),
        )?);
        match &cancelled.ack {
            agent_contracts::TurnCancelAck::Cancelled {
                task_id: cancelled_task,
                ..
            } => assert_eq!(
                cancelled_task.as_ref(),
                Some(&task_id),
                "the cancel barrier names the journey's task"
            ),
            other => {
                panic!("the held turn must be cancelled with a durable barrier, got {other:?}")
            }
        }
        assert!(cancelled.identity_mismatch.is_none());
        // The parked model usually settles through the cancellation token
        // before this release fires; a gone receiver is that happy race.
        let _ = cancel_release.send(true);
        let after_cancel = drive_turn_to_idle(&mut stream, "the cancel settle").await?;
        assert_eq!(
            readiness_of(&after_cancel),
            WorkContinueReason::Ready,
            "a cancelled continuation keeps the task and its retained directive"
        );
        wait_event(
            &mut events,
            |event| {
                matches!(
                    event,
                    RuntimeEvent::TurnCancelled {
                        task_id: Some(cancelled),
                        ..
                    } if *cancelled == task_id
                )
            },
            "report the cancelled turn",
        )
        .await;
        eprintln!("t7[session-1]: mid-turn cancel landed with a durable barrier");

        // 5. SAVE: a formal cross-plane checkpoint in the run's own store.
        let captured = expect_value(exchange::<_, _, WorkCheckpointResponse>(
            &mut stream,
            &request("work", "checkpoint", WorkCheckpointRequest {}),
        )?);
        assert_eq!(captured.run_id, run_id_1);
        assert!(captured.payload_bytes > 0);
        assert!(captured.tasks >= 1, "the artifact carries the task rows");
        let artifact_on_disk = root
            .join(".focus-agent")
            .join("checkpoints")
            .join(&captured.artifact);
        assert!(
            artifact_on_disk.exists(),
            "the checkpoint artifact exists on disk: {}",
            artifact_on_disk.display()
        );
        eprintln!("t7[session-1]: checkpoint {} saved", captured.artifact);
        artifact_name = captured.artifact.clone();

        drop(stream);
        fixture.composed.shutdown().await?;
        stop_and_join_bounded(server).await?;
        eprintln!("t7[session-1]: host shut down");
        // The fixture (and its SingleInstance lock) drops here, so session 2
        // composes over the same workspace as a genuinely NEW process-side
        // instance.
    }

    // ============================ SESSION 2 ============================
    // 6. COLD RECOVERY: a new composition + a new host over the SAME
    //    workspace. The scripted model continues at round 6 (the remaining
    //    work), like the established restore harnesses.
    let model = JourneyModel::new(6, Vec::new());
    let fixture = journey_workspace(&root, model).await?;
    let mut events = fixture.composed.subscribe();
    let handle: RuntimeHandle = fixture.composed.handle().clone();
    fixture.composed.instance.start().await?;
    let run_id_2 = handle.run_id();
    assert_ne!(run_id_1, run_id_2, "a restarted run is a new run");
    let server = start_server(&fixture, endpoint).await?;
    let mut stream = connect(&server.endpoint).await;
    eprintln!("t7[session-2]: connected; run {}", run_id_2);

    let cold = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert_eq!(cold.run_id, run_id_2);
    assert!(
        cold.focus.is_none() && cold.tasks.is_empty(),
        "before the restore the new process knows NO tasks (a genuinely cold run)"
    );

    // 7. RESTORE the captured artifact: the response names the run the
    //    checkpoint was captured under — the lineage link — and the plane
    //    comes back with the SAME task focused.
    let restored = expect_value(exchange::<_, _, WorkRestoreResponse>(
        &mut stream,
        &request(
            "work",
            "restore",
            WorkRestoreRequest {
                artifact: Some(artifact_name.clone()),
            },
        ),
    )?);
    assert_eq!(restored.artifact, artifact_name);
    assert_eq!(
        restored.restored_run_id, run_id_1,
        "the restored checkpoint was captured under session 1's run — one lineage"
    );
    assert!(
        restored.evidence_degraded.is_empty(),
        "no evidence degradation on this restore: {:?}",
        restored.evidence_degraded
    );
    wait_event(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;

    let verified = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert_eq!(
        verified.focus.as_ref().map(|focus| focus.task_id),
        Some(task_id),
        "the SAME TaskId is focused again after the cold restore"
    );
    assert_eq!(readiness_of(&verified), WorkContinueReason::Ready);

    // 8. EXTERNAL CHANGES SURVIVED the process boundary: the workspace
    //    files session 1 produced are still exactly what was written, and
    //    the committed effect was NOT replayed (exactly one part_a change
    //    row in the durable journal).
    assert_eq!(std::fs::read_to_string(root.join("part_a.md"))?, PART_A);
    assert_eq!(
        std::fs::read_to_string(root.join("part_b.md"))?,
        format!("Q: {PART_A}")
    );
    assert_eq!(
        change_rows(&root, "part_a.md"),
        1,
        "the restored lineage must not re-execute an already committed effect"
    );

    // 9. CONTINUE the same task: the remaining goal work (summary.md) runs
    //    through real tools again — fs.read part_b.md, fs.write summary.md.
    let resumed = expect_value(exchange::<_, _, WorkContinueResponse>(
        &mut stream,
        &request(
            "work",
            "continue",
            WorkContinueRequest {
                expected_task_id: Some(task_id),
            },
        ),
    )?);
    assert_eq!(resumed.disposition, WorkContinueDisposition::Continued);
    assert_eq!(resumed.task_id, Some(task_id));
    drive_turn_to_idle(&mut stream, "the post-restore continuation").await?;
    let summary = std::fs::read_to_string(root.join("summary.md"))?;
    assert_eq!(
        summary,
        format!("summary of: Q: {PART_A}"),
        "summary.md must reference part_b's real line (via fs.read) under the SAME task"
    );
    assert_eq!(
        change_rows(&root, "part_a.md"),
        1,
        "the continuation adds only the remaining work; nothing replays"
    );
    eprintln!("t7[session-2]: remaining work completed -> summary = {summary:?}");

    // 10. DELIVERY under the default OperatorClosureOnly policy: the final
    //     ended the turn, NOT the task — the task stays Active awaiting the
    //     operator, with the anchor (goal verbatim) intact across the whole
    //     lineage.
    let detail = expect_value(exchange::<_, _, WorkTaskDetailResponse>(
        &mut stream,
        &request("work", "task_detail", WorkTaskDetailRequest { task_id }),
    )?);
    assert_eq!(detail.task_id, task_id);
    assert_eq!(
        detail.goal, GOAL,
        "the anchor carries the submitted goal verbatim"
    );
    assert_eq!(
        detail.status,
        TaskSnapshotStatus::Active,
        "OperatorClosureOnly: an ordinary final is not a durable completion"
    );

    // The operator closes the task through the LOCAL operator surface (the
    // same explicit actor command the TUI exposes); the typed completion
    // event carries the operator summary and the final output's digest.
    handle
        .complete_current_task(OPERATOR_SUMMARY.into())
        .await?;
    let completed = wait_event(
        &mut events,
        |event| {
            matches!(
                event,
                RuntimeEvent::TaskCompleted {
                    task_id: closed,
                    ..
                } if *closed == task_id
            )
        },
        "report the durable TaskCompleted",
    )
    .await;
    match &completed {
        RuntimeEvent::TaskCompleted {
            summary,
            final_output_digest,
            ..
        } => {
            assert_eq!(summary, OPERATOR_SUMMARY);
            assert!(
                final_output_digest.is_some(),
                "the delivery record binds the turn's final output by digest"
            );
        }
        other => panic!("expected TaskCompleted, got {other:?}"),
    }

    let closed_detail = expect_value(exchange::<_, _, WorkTaskDetailResponse>(
        &mut stream,
        &request("work", "task_detail", WorkTaskDetailRequest { task_id }),
    )?);
    assert_eq!(
        closed_detail.status,
        TaskSnapshotStatus::Completed,
        "only the explicit operator closure durably completes the task"
    );
    let lookup = expect_value(exchange::<_, _, WorkTaskCompletionResponse>(
        &mut stream,
        &request(
            "work",
            "task_completion",
            WorkTaskCompletionRequest { task_id },
        ),
    )?);
    assert_eq!(
        lookup.task_id, task_id,
        "the completion record is queryable after closure"
    );
    eprintln!("t7[session-2]: delivery verified; task closed by the operator");

    drop(stream);
    fixture.composed.shutdown().await?;
    stop_and_join_bounded(server).await?;
    eprintln!("t7[session-2]: done");
    Ok(())
}

// ---------------------------------------------------------------------------
// T7 failure variant: after the cold restore, one transient REAL tool
// failure must not lose the task state; the retry completes the same write.
// ---------------------------------------------------------------------------

async fn transient_failure_after_restore(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();

    // ---- Session 1: stage part_a.md, save, exit. ----
    let task_id;
    let artifact_name;
    let run_id_1;
    {
        let fixture = journey_workspace(&root, FailureModel::new(0)).await?;
        let _events = fixture.composed.subscribe();
        let handle: RuntimeHandle = fixture.composed.handle().clone();
        fixture.composed.instance.start().await?;
        run_id_1 = handle.run_id();
        let server = start_server(&fixture, endpoint.clone()).await?;
        let mut stream = connect(&server.endpoint).await;

        let submitted = expect_value(exchange::<_, _, WorkSubmitResponse>(
            &mut stream,
            &request(
                "work",
                "submit",
                WorkSubmitRequest {
                    goal: FAILURE_GOAL.into(),
                    client_request_id: "t7-failure-1".into(),
                },
            ),
        )?);
        assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
        task_id = submitted.task_id;
        drive_turn_to_idle(&mut stream, "the staging turn").await?;
        assert_eq!(
            std::fs::read_to_string(root.join("part_a.md"))?,
            FAILURE_PART_A
        );

        let captured = expect_value(exchange::<_, _, WorkCheckpointResponse>(
            &mut stream,
            &request("work", "checkpoint", WorkCheckpointRequest {}),
        )?);
        assert_eq!(captured.run_id, run_id_1);
        artifact_name = captured.artifact.clone();

        drop(stream);
        fixture.composed.shutdown().await?;
        stop_and_join_bounded(server).await?;
    }
    eprintln!("t7-failure[session-1]: staged and shut down");

    // ---- Session 2: cold restore; the FIRST continuation fails on a real
    // tool condition (archive/ has no parent yet); the task state survives;
    // resolving the condition and continuing again completes the write. ----
    let fixture = journey_workspace(&root, FailureModel::new(2)).await?;
    let mut events = fixture.composed.subscribe();
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint).await?;
    let mut stream = connect(&server.endpoint).await;

    let restored = expect_value(exchange::<_, _, WorkRestoreResponse>(
        &mut stream,
        &request(
            "work",
            "restore",
            WorkRestoreRequest {
                artifact: Some(artifact_name),
            },
        ),
    )?);
    assert_eq!(restored.restored_run_id, run_id_1);
    wait_event(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;
    let verified = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert_eq!(
        verified.focus.as_ref().map(|focus| focus.task_id),
        Some(task_id),
        "the same task comes back from the cold restore"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("part_a.md"))?,
        FAILURE_PART_A,
        "the external change survived the restart"
    );

    // First attempt: the write to archive/summary.md is approved over the
    // wire and then REFUSED by the real filesystem (parent_path_not_found).
    // The failure reaches the model as a tool result; the turn ends with a
    // plain final and the task state is untouched.
    let resumed = expect_value(exchange::<_, _, WorkContinueResponse>(
        &mut stream,
        &request(
            "work",
            "continue",
            WorkContinueRequest {
                expected_task_id: Some(task_id),
            },
        ),
    )?);
    assert_eq!(resumed.disposition, WorkContinueDisposition::Continued);
    drive_turn_to_idle(&mut stream, "the failing attempt").await?;
    assert!(
        !root.join("archive").exists() && !root.join("archive/summary.md").exists(),
        "the refused write must not have created anything"
    );
    let after_failure = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &snapshot_request(),
    )?);
    assert_eq!(
        after_failure.focus.as_ref().map(|focus| focus.task_id),
        Some(task_id),
        "the transient failure did not lose the task"
    );
    assert_eq!(
        readiness_of(&after_failure),
        WorkContinueReason::Ready,
        "the retained directive survives the failed attempt"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("part_a.md"))?,
        FAILURE_PART_A,
        "the committed part_a effect was not disturbed or replayed"
    );
    eprintln!("t7-failure[session-2]: transient tool failure absorbed; task intact");

    // Resolve the external condition, then retry: the SAME write now lands.
    std::fs::create_dir_all(root.join("archive"))?;
    let retried = expect_value(exchange::<_, _, WorkContinueResponse>(
        &mut stream,
        &request(
            "work",
            "continue",
            WorkContinueRequest {
                expected_task_id: Some(task_id),
            },
        ),
    )?);
    assert_eq!(retried.disposition, WorkContinueDisposition::Continued);
    drive_turn_to_idle(&mut stream, "the retry").await?;
    assert_eq!(
        std::fs::read_to_string(root.join("archive").join("summary.md"))?,
        FAILURE_PART_A,
        "the retried write completed the remaining work under the same task"
    );
    assert_eq!(
        change_rows(&root, "part_a.md"),
        1,
        "the retry must not have replayed the committed part_a effect"
    );

    // Still Active: even a successful retry is the model's ordinary final —
    // durable closure stays with the operator.
    let detail = expect_value(exchange::<_, _, WorkTaskDetailResponse>(
        &mut stream,
        &request("work", "task_detail", WorkTaskDetailRequest { task_id }),
    )?);
    assert_eq!(detail.status, TaskSnapshotStatus::Active);
    assert_eq!(detail.goal, FAILURE_GOAL);

    drop(stream);
    fixture.composed.shutdown().await?;
    stop_and_join_bounded(server).await?;
    eprintln!("t7-failure[session-2]: done");
    Ok(())
}

// multi_thread on purpose: the client side uses blocking std IO over the
// local transport; on the default current_thread test runtime that would
// freeze the runtime and deadlock the server-side block_on calls.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_t7_same_task_full_backend_journey() {
    same_task_journey(LocalEndpoint::NamedPipe(format!(
        "focus-agent-t7-journey-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_t7_same_task_full_backend_journey() {
    same_task_journey(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-t7-journey-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_t7_transient_failure_after_restore_retries_to_completion() {
    transient_failure_after_restore(LocalEndpoint::NamedPipe(format!(
        "focus-agent-t7-failure-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_t7_transient_failure_after_restore_retries_to_completion() {
    transient_failure_after_restore(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-t7-failure-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(not(any(windows, unix)))]
compile_error!("the host t7 journey requires a local transport");

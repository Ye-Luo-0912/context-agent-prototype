//! P3+G2 end-to-end: a real composed runtime served by the host over a
//! local transport, exercised with the shared framing and the C0 wire
//! shapes — submit, idempotent retry, conflict, snapshot, approval respond,
//! cancel, unsupported route.
//!
//! Each transport gets its own test: UDS on Linux (CI), Named Pipe on
//! Windows (the desktop client's platform). The flow is identical, which is
//! the point: OS backends are isolated, everything above them is not.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_compose::{
    ComposeConfig, ContextPolicy, HostToolPolicyRegistry, build_context_engine, compose,
};
use agent_contracts::ApprovalGate as _;
use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
use agent_host::{
    HostPlane, HostServer, LocalEndpoint, SingleInstance, negotiated_profile, session_schema_digest,
};
use agent_platform_protocol::{
    ActiveFeatures, ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse,
    Causality, EnvelopeKind, MessageId, PlatformEnvelope, PlatformResponse, ProtocolIdentity,
    ProtocolVersion, RequestId, Route, WorkCancelRequest, WorkCancelResponse, WorkContinueRequest,
    WorkContinueResponse, WorkEventNotification, WorkSnapshotRequest, WorkSnapshotResponse,
    WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse, WorkSubscribeRequest,
    WorkSubscribeResponse,
};
use agent_runtime::{RuntimeHandle, WorkControlSessionRegistry};
use serde_json::json;

struct Composed {
    composed: agent_compose::ComposedRuntime,
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
    _lock: SingleInstance,
}

async fn compose_workspace(root: &std::path::Path) -> anyhow::Result<Composed> {
    let workspace = agent_workspace::Workspace::open(root).await?;
    let lock = SingleInstance::acquire(workspace.state_dir())?;
    let model: Arc<dyn agent_contracts::ModelTransport> =
        Arc::new(agent_compose::MockModelTransport);
    let context_engine = build_context_engine(
        ContextPolicy::Rolling,
        workspace.state_dir(),
        Some(model.clone()),
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
    let composed = compose(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate.clone() as Arc<dyn agent_contracts::ApprovalGate>,
        base_tools,
        capability_aware: true,
        journal: None,
        artifact_store: None,
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: None,
        verification_recipes: Some(verification_recipes),
        project_proof_refresh: false,
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
    .await?;
    Ok(Composed {
        composed,
        broker,
        gate,
        _lock: lock,
    })
}

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

/// One framed exchange: write a request, read and correlate the response.
fn exchange<S: Read + Write, P: serde::Serialize, R: serde::de::DeserializeOwned>(
    stream: &mut S,
    request: &PlatformEnvelope<P>,
) -> anyhow::Result<PlatformResponse<R>> {
    agent_host::write_frame(stream, &serde_json::to_vec(request)?)?;
    read_response(stream, &request.request_id)
}

/// Reads one framed response and checks it correlates with the request.
fn read_response<S: Read + Write, R: serde::de::DeserializeOwned>(
    stream: &mut S,
    request_id: &Option<RequestId>,
) -> anyhow::Result<PlatformResponse<R>> {
    let frame = agent_host::read_frame(stream)?
        .ok_or_else(|| anyhow::anyhow!("connection closed before response"))?;
    let envelope = serde_json::from_slice::<PlatformEnvelope<PlatformResponse<R>>>(&frame)?;
    assert_eq!(&envelope.request_id, request_id, "correlated response");
    assert_eq!(envelope.kind, EnvelopeKind::Response);
    Ok(envelope.payload)
}

/// One full work-plane drill over the given local endpoint. The stream type
/// comes from the per-platform [`connect`] helper, so the Windows named-pipe
/// and the Unix UDS backends run the identical flow.
async fn run_e2e(endpoint: LocalEndpoint, label: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    let handle: RuntimeHandle = fixture.composed.handle().clone();
    let registry = WorkControlSessionRegistry::new(handle.run_id());
    let plane = HostPlane {
        profile: negotiated_profile()?,
        handle,
        broker: Arc::clone(&fixture.broker),
        gate: Arc::clone(&fixture.gate),
        registry,
    };

    fixture.composed.instance.start().await?;

    let stop = Arc::new(AtomicBool::new(false));
    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: false,
        stop: Arc::clone(&stop),
    };
    let runtime = tokio::runtime::Handle::current();
    let serve_thread = std::thread::spawn(move || server.serve(plane, runtime));
    // Give the accept loop a beat to bind.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut stream = connect(&endpoint).await;
    eprintln!("e2e[{label}]: connected");

    // 1. submit: acceptance receipt with task binding.
    let submitted = expect_value(exchange::<_, _, WorkSubmitResponse>(
        &mut stream,
        &request(
            "work",
            "submit",
            WorkSubmitRequest {
                goal: "host e2e: fix the flaky retry test".into(),
                client_request_id: "e2e-1".into(),
            },
        ),
    )?);
    assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
    let task_id = submitted.task_id.to_string();
    eprintln!("e2e[{label}]: submitted -> task {task_id}");

    // 2. idempotent retry: same client_request_id + same goal.
    let retry = expect_value(exchange::<_, _, WorkSubmitResponse>(
        &mut stream,
        &request(
            "work",
            "submit",
            WorkSubmitRequest {
                goal: "host e2e: fix the flaky retry test".into(),
                client_request_id: "e2e-1".into(),
            },
        ),
    )?);
    assert_eq!(retry.disposition, WorkSubmitDisposition::AlreadyAccepted);

    // 3. conflicting reuse: same id, different goal -> structured rejection.
    let conflict = exchange::<_, _, WorkSubmitResponse>(
        &mut stream,
        &request(
            "work",
            "submit",
            WorkSubmitRequest {
                goal: "host e2e: different goal".into(),
                client_request_id: "e2e-1".into(),
            },
        ),
    )?;
    match conflict {
        PlatformResponse::Error { error } => assert_eq!(error.code, "work.rejected"),
        PlatformResponse::Success { .. } => panic!("conflicting reuse must be rejected"),
    }

    // 4. snapshot: the focused task is visible.
    let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &request("work", "snapshot", WorkSnapshotRequest {}),
    )?);
    assert!(snapshot.run_started);
    let focus = snapshot.focus.as_ref().expect("focus after submit");
    assert_eq!(focus.task_id.to_string(), task_id);

    // 5. approval flow: inject one pending request through the live broker,
    // see it in the snapshot, answer it, see the real delivery receipt.
    // The honest injector: a real tool call through the interactive gate,
    // exactly as Core would drive it. The authorize future stays parked
    // until the client's approval response resolves it.
    let gate = Arc::clone(&fixture.gate);
    let authorize = tokio::spawn(async move {
        gate.authorize(
            &agent_contracts::ToolCall {
                id: "call_e2e_1".into(),
                name: "fs.write".into(),
                arguments: json!({"path": "src/lib.rs"}),
            },
            &agent_contracts::ToolSpec {
                name: "fs.write".into(),
                description: "e2e".into(),
                input_schema: json!({}),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
                output_budget: None,
                roles: Vec::new(),
            },
            &agent_contracts::CancellationToken::new(),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &request("work", "snapshot", WorkSnapshotRequest {}),
    )?);
    // The gate mints its own request id; the client takes the server-bound
    // id from the snapshot, exactly like the real approval card flow.
    assert!(
        !snapshot.pending_approvals.is_empty(),
        "pending approval must be visible in the snapshot"
    );
    let bound_id = snapshot.pending_approvals[0].request_id.clone();
    let answered = expect_value(exchange::<_, _, ApprovalRespondResponse>(
        &mut stream,
        &request(
            "approval",
            "respond",
            ApprovalRespondRequest {
                request_id: bound_id,
                decision: agent_contracts::ApprovalDecision::Allow,
            },
        ),
    )?);
    assert_eq!(answered.outcome, ApprovalRespondOutcome::Delivered);
    let gate_decision = authorize
        .await
        .expect("authorize task")
        .expect("gate result");
    assert_eq!(gate_decision, agent_contracts::ApprovalDecision::Allow);

    // 6. cancel: either truth is honest (no turn was started by submit).
    let cancel = expect_value(exchange::<_, _, WorkCancelResponse>(
        &mut stream,
        &request("work", "cancel", WorkCancelRequest {}),
    )?);
    assert!(matches!(
        cancel.ack,
        agent_contracts::TurnCancelAck::Cancelled { .. }
            | agent_contracts::TurnCancelAck::NoActiveTurn
    ));

    // 7. unsupported route answers the structured error.
    match exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &request("work", "archive", WorkSnapshotRequest {}),
    )? {
        PlatformResponse::Error { error } => assert_eq!(error.code, "route.unsupported"),
        PlatformResponse::Success { .. } => panic!("unknown route must not succeed"),
    }

    drop(stream);
    fixture.composed.shutdown().await?;
    // Honest teardown: the serve loop must still be alive here, so its joined
    // result is a real assertion, not a discarded one. Set the stop flag,
    // wake the parked accept with one throwaway connection, then require the
    // thread to have exited `Ok` — a serve loop that died mid-test (masked
    // accept-loop failures included) fails this check.
    stop.store(true, Ordering::SeqCst);
    let _ = connect(&endpoint).await;
    let serve_result = serve_thread
        .join()
        .map_err(|_| anyhow::anyhow!("serve thread panicked"))?;
    serve_result?;
    eprintln!("e2e[{label}]: done");
    Ok(())
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
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!(
        "named pipe {path} never became connectable after {:?}",
        started.elapsed()
    );
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
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!(
        "uds socket {} never became connectable after {:?}",
        path.display(),
        started.elapsed()
    );
}

#[cfg(windows)]
fn connect_blocking(endpoint: &LocalEndpoint) -> anyhow::Result<std::fs::File> {
    let LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    let path = format!(r"\\.\pipe\{name}");
    for _ in 0..50 {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            return Ok(file);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("named pipe {path} never became connectable")
}

#[cfg(unix)]
fn connect_blocking(endpoint: &LocalEndpoint) -> anyhow::Result<std::os::unix::net::UnixStream> {
    let LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    for _ in 0..50 {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(path) {
            return Ok(stream);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("uds socket {} never became connectable", path.display())
}

// ---------------------------------------------------------------------------
// N1: long-lived host — bounded stop, grant revocation, endpoint safety.
// ---------------------------------------------------------------------------

/// Upper bound for a full stop: the host itself bounds the wind-down at
/// 2 x SHUTDOWN_GRACE plus the router's per-request deadline; the assertion
/// here just needs to be comfortably above that.
const BOUNDED_STOP: std::time::Duration = std::time::Duration::from_secs(20);

/// One host serving one endpoint on a live composed runtime.
struct TestServer {
    endpoint: LocalEndpoint,
    stop: Arc<AtomicBool>,
    registry: Arc<WorkControlSessionRegistry>,
    serve: std::thread::JoinHandle<anyhow::Result<()>>,
}

async fn start_server(
    fixture: &Composed,
    endpoint: LocalEndpoint,
    probe: bool,
) -> anyhow::Result<TestServer> {
    let handle: RuntimeHandle = fixture.composed.handle().clone();
    let registry = WorkControlSessionRegistry::new(handle.run_id());
    let plane = HostPlane {
        profile: negotiated_profile()?,
        handle,
        broker: Arc::clone(&fixture.broker),
        gate: Arc::clone(&fixture.gate),
        registry: Arc::clone(&registry),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: false,
        stop: Arc::clone(&stop),
    };
    let runtime = tokio::runtime::Handle::current();
    let serve = std::thread::spawn(move || server.serve(plane, runtime));
    let server = TestServer {
        endpoint,
        stop,
        registry,
        serve,
    };
    if probe {
        // One throwaway connection proves the endpoint is bound before the
        // test proceeds; it is served and closed like any client. If the
        // serve loop died before binding (e.g. the endpoint could not be
        // created), fail right now with its actual error instead of
        // waiting out the full connect budget on a pipe that can never
        // appear.
        let started = std::time::Instant::now();
        let mut connected = false;
        while started.elapsed() < CONNECT_BUDGET {
            if server.serve.is_finished() {
                let error = match server.serve.join() {
                    Ok(Ok(())) => "exited cleanly without ever binding".to_string(),
                    Ok(Err(error)) => format!("failed: {error:#}"),
                    Err(_) => "panicked".to_string(),
                };
                anyhow::bail!(
                    "host serve loop died before the endpoint became connectable: {error}"
                );
            }
            if try_connect_once(&server.endpoint) {
                connected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(
            connected,
            "endpoint {:?} never became connectable within {CONNECT_BUDGET:?} (waited {:?}); \
             the serve thread is {}",
            server.endpoint,
            started.elapsed(),
            if server.serve.is_finished() {
                "dead (see earlier failure)"
            } else {
                "alive but never bound"
            }
        );
    }
    Ok(server)
}

/// The connect budget for the readiness probe: comfortably above any slow
/// runner's scheduling jitter, so a real failure is never mistaken for
/// slowness.
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// One immediate, allocation-free connection attempt; `false` when the
/// endpoint is not (yet) accepting.
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

/// The stop-path assertion: set the cooperative stop flag, poke the parked
/// accept loop with one throwaway connection (the same stop/wake mechanism
/// the real Ctrl-C path uses — msys cannot deliver CTRL_C), and require the
/// serve thread to have exited `Ok` within `bound`. Returns the elapsed
/// time and the registry so callers can assert post-shutdown facts.
async fn stop_and_join_bounded(
    server: TestServer,
    bound: std::time::Duration,
) -> anyhow::Result<(std::time::Duration, Arc<WorkControlSessionRegistry>)> {
    let started = std::time::Instant::now();
    server.stop.store(true, Ordering::SeqCst);
    let _ = connect(&server.endpoint).await;
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(server.serve.join());
    });
    match done_rx.recv_timeout(bound) {
        Ok(joined) => {
            joined.map_err(|_| anyhow::anyhow!("serve thread panicked"))??;
            Ok((started.elapsed(), server.registry))
        }
        Err(_) => Err(anyhow::anyhow!(
            "serve loop did not exit within {bound:?} (still running after {:?})",
            started.elapsed()
        )),
    }
}

/// Polls until every installed session grant has been revoked (the
/// registry must return to its baseline) within the deadline.
async fn assert_registry_drained(registry: &WorkControlSessionRegistry) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while registry.live_sessions() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "session grants did not revoke: {} still installed",
            registry.live_sessions()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        registry.live_sessions(),
        0,
        "registry must return to baseline"
    );
}

/// Criterion 1: two parallel clients are served concurrently, each
/// connection getting its own correlated answer on its own wire regardless
/// of order. The mutation goes through client A only — the runtime has a
/// single actor, so a second concurrent submit would be refused by design
/// ("a turn is already running"); B exercises the served read plane while
/// A's submit is in flight.
async fn two_clients_serve_independently(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;

    let mut client_a = connect(&server.endpoint).await;
    let mut client_b = connect(&server.endpoint).await;
    let request_a = request(
        "work",
        "submit",
        WorkSubmitRequest {
            goal: "host e2e: client a".into(),
            client_request_id: "two-a".into(),
        },
    );
    let request_b = request("work", "snapshot", WorkSnapshotRequest {});
    agent_host::write_frame(&mut client_a, &serde_json::to_vec(&request_a)?)?;
    agent_host::write_frame(&mut client_b, &serde_json::to_vec(&request_b)?)?;
    // Read B first: B's snapshot answer must arrive on B's wire with B's
    // correlation even though A connected and wrote first — no cross-talk,
    // no queueing behind the other connection.
    let answered_b: PlatformResponse<WorkSnapshotResponse> =
        read_response(&mut client_b, &request_b.request_id)?;
    let answered_a: PlatformResponse<WorkSubmitResponse> =
        read_response(&mut client_a, &request_a.request_id)?;
    let snapshot_b = expect_value(answered_b);
    let submitted_a = expect_value(answered_a);
    assert_eq!(submitted_a.disposition, WorkSubmitDisposition::Accepted);
    assert!(snapshot_b.run_started, "B is served on the live run");

    // And in the other order: A's served read does not disturb B's wire.
    let follow_up_a = request("work", "snapshot", WorkSnapshotRequest {});
    let follow_up_b = request("work", "snapshot", WorkSnapshotRequest {});
    agent_host::write_frame(&mut client_a, &serde_json::to_vec(&follow_up_a)?)?;
    agent_host::write_frame(&mut client_b, &serde_json::to_vec(&follow_up_b)?)?;
    let second_b: PlatformResponse<WorkSnapshotResponse> =
        read_response(&mut client_b, &follow_up_b.request_id)?;
    let second_a: PlatformResponse<WorkSnapshotResponse> =
        read_response(&mut client_a, &follow_up_a.request_id)?;
    assert!(expect_value(second_b).run_started);
    assert!(expect_value(second_a).run_started);

    drop(client_a);
    drop(client_b);
    fixture.composed.shutdown().await?;
    stop_and_join_bounded(server, BOUNDED_STOP).await?;
    Ok(())
}

/// Criterion 2: sequential connect/disconnect past the old 64-slot cap —
/// every disconnect must revoke its session grant, so the registry returns
/// to its baseline instead of the 65th install panicking the accept loop.
async fn grants_revoke_on_disconnect(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;
    assert_eq!(
        server.registry.live_sessions(),
        0,
        "a fresh host starts at the baseline"
    );

    for _ in 0..70 {
        let mut stream = connect(&server.endpoint).await;
        let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
            &mut stream,
            &request("work", "snapshot", WorkSnapshotRequest {}),
        )?);
        assert!(snapshot.run_started);
        drop(stream);
    }

    assert_registry_drained(&server.registry).await;
    fixture.composed.shutdown().await?;
    stop_and_join_bounded(server, BOUNDED_STOP).await?;
    Ok(())
}

/// Criterion 3a: with no client at all, the stop path exits in bounded time.
async fn stop_is_bounded_with_no_client(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;
    let (elapsed, registry) = stop_and_join_bounded(server, BOUNDED_STOP).await?;
    eprintln!("stop with no client took {elapsed:?}");
    assert_registry_drained(&registry).await;
    fixture.composed.shutdown().await?;
    Ok(())
}

/// Criterion 3b: a half-frame client (parked mid-header read) and a
/// slow-read client (answered, then parked waiting for a frame that never
/// comes) must not hold the stop path past its bound. Both connections stay
/// open across the stop; the wind-down settles then cancels them.
async fn stop_is_bounded_with_hostile_clients(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;

    // Half-frame: two bytes of the four-byte header, then silence.
    let mut half_frame = connect(&server.endpoint).await;
    half_frame.write_all(&[0x00, 0x00])?;
    // Slow reader: a full request whose response is never read.
    let mut slow_reader = connect(&server.endpoint).await;
    agent_host::write_frame(
        &mut slow_reader,
        &serde_json::to_vec(&request("work", "snapshot", WorkSnapshotRequest {}))?,
    )?;
    // Let both workers reach their parked reads.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    fixture.composed.shutdown().await?;
    let (elapsed, registry) = stop_and_join_bounded(server, BOUNDED_STOP).await?;
    eprintln!("stop with hostile clients took {elapsed:?}");
    // The shutdown-driven cancel path must release both grants too.
    assert_registry_drained(&registry).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// N3: the subscribed event stream reaches the connection that asked for it.
// ---------------------------------------------------------------------------

/// One inbound frame, demultiplexed: server-initiated event notifications
/// are decoded straight into the typed [`WorkEventNotification`] DTO, while
/// responses keep their correlation id for the caller to match. Every frame
/// is decoded strictly, so a single interleaved or corrupted byte fails the
/// test instead of smuggling a wrong message through.
enum Incoming {
    Response {
        request_id: Option<RequestId>,
        payload: serde_json::Value,
    },
    Notification(Box<PlatformEnvelope<WorkEventNotification>>),
}

/// Upper bound for one parked read in the notification loops: a regression
/// that stops event delivery must fail the test, not hang it.
const NOTIFICATION_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// The e2e stream types can each produce an independent handle to the same
/// connection, which is how a parked read gets bounded from the side.
trait TryCloneStream: Read + Write + Sized {
    // Windows-side event drills clone the stream; unix drills only need the
    // read deadline, so on unix this method is intentionally uncalled.
    #[allow(dead_code)]
    fn try_clone_stream(&self) -> anyhow::Result<Self>;

    /// Bounds the NEXT read on this connection so a silent peer fails the
    /// test instead of hanging it.
    fn arm_read_deadline(&self, bound: std::time::Duration) -> anyhow::Result<()>;
}

#[cfg(windows)]
impl TryCloneStream for std::fs::File {
    fn try_clone_stream(&self) -> anyhow::Result<Self> {
        Ok(self.try_clone()?)
    }

    fn arm_read_deadline(&self, _bound: std::time::Duration) -> anyhow::Result<()> {
        // std has no pipe read deadline; the CancelIoEx watchdog in
        // [`read_incoming_bounded`] provides the bound instead.
        Ok(())
    }
}

#[cfg(unix)]
impl TryCloneStream for std::os::unix::net::UnixStream {
    fn try_clone_stream(&self) -> anyhow::Result<Self> {
        Ok(self.try_clone()?)
    }

    fn arm_read_deadline(&self, bound: std::time::Duration) -> anyhow::Result<()> {
        self.set_read_timeout(Some(bound))?;
        Ok(())
    }
}

/// Reads one incoming frame with a bounded wait; `Ok(None)` when the bound
/// expired without a frame. On Windows the read is interrupted with
/// `CancelIoEx` through a duplicate of the pipe's file object (the same
/// file-object rule the host's request loop works around); on Unix a read
/// timeout is installed on the socket through the duplicate.
#[cfg(windows)]
fn read_incoming_bounded<S>(
    stream: &mut S,
    bound: std::time::Duration,
) -> anyhow::Result<Option<Incoming>>
where
    S: TryCloneStream + std::os::windows::io::AsRawHandle + Send + 'static,
{
    stream.arm_read_deadline(bound)?;
    let interrupt = stream.try_clone_stream()?;
    std::thread::spawn(move || {
        std::thread::sleep(bound);
        unsafe {
            windows_sys::Win32::System::IO::CancelIoEx(
                interrupt.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                std::ptr::null(),
            );
        }
    });
    match read_incoming(stream) {
        Ok(incoming) => Ok(Some(incoming)),
        Err(error) => {
            let bounded = error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                io.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED as i32)
            });
            if bounded { Ok(None) } else { Err(error) }
        }
    }
}

#[cfg(unix)]
fn read_incoming_bounded<S: TryCloneStream>(
    stream: &mut S,
    bound: std::time::Duration,
) -> anyhow::Result<Option<Incoming>> {
    stream.arm_read_deadline(bound)?;
    match read_incoming(stream) {
        Ok(incoming) => Ok(Some(incoming)),
        Err(error) => {
            let bounded = error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            });
            if bounded { Ok(None) } else { Err(error) }
        }
    }
}

fn read_incoming<S: Read + Write>(stream: &mut S) -> anyhow::Result<Incoming> {
    let frame = agent_host::read_frame(stream)?
        .ok_or_else(|| anyhow::anyhow!("connection closed before a frame"))?;
    let envelope = serde_json::from_slice::<PlatformEnvelope<serde_json::Value>>(&frame)?;
    if envelope.kind == EnvelopeKind::Notification {
        assert_eq!(
            (
                envelope.route.namespace.as_str(),
                envelope.route.operation.as_str()
            ),
            ("work", "event"),
            "notifications must arrive on the work/event route"
        );
        assert!(
            envelope.request_id.is_none(),
            "a notification must not carry request correlation"
        );
        let typed = serde_json::from_slice::<PlatformEnvelope<WorkEventNotification>>(&frame)
            .expect("notification frame must decode into the typed DTO");
        return Ok(Incoming::Notification(Box::new(typed)));
    }
    assert_eq!(
        envelope.kind,
        EnvelopeKind::Response,
        "unexpected frame kind"
    );
    Ok(Incoming::Response {
        request_id: envelope.request_id,
        payload: envelope.payload,
    })
}

/// Reads frames until the response matching `request_id` arrives,
/// collecting any event notifications seen along the way — the server may
/// legitimately interleave notifications between a request and its answer.
fn read_response_collecting<R: serde::de::DeserializeOwned>(
    stream: &mut (impl Read + Write),
    request_id: &Option<RequestId>,
    notifications: &mut Vec<PlatformEnvelope<WorkEventNotification>>,
) -> anyhow::Result<PlatformResponse<R>> {
    loop {
        match read_incoming(stream)? {
            Incoming::Notification(notification) => notifications.push(*notification),
            Incoming::Response {
                request_id: got,
                payload,
            } => {
                assert_eq!(&got, request_id, "correlated response");
                return Ok(serde_json::from_value(payload)?);
            }
        }
    }
}

/// Checks one notification against the subscribe handshake's contract:
/// typed envelope, this run, non-decreasing cursor, and the B1 kind split —
/// a durable fact is strictly above the snapshot watermark (no loss, no
/// double-count against the snapshot), while live-only progress repeats the
/// preceding durable cursor and may sit at or below it.
fn assert_notification(
    notification: &PlatformEnvelope<WorkEventNotification>,
    run_id: agent_contracts::RunId,
    watermark: u64,
    previous_seq: Option<u64>,
) {
    assert_eq!(notification.kind, EnvelopeKind::Notification);
    assert!(notification.route.is_work_event());
    assert!(notification.request_id.is_none());
    let envelope = &notification.payload.envelope;
    assert_eq!(envelope.run_id, run_id, "events must carry this run's id");
    assert!(
        envelope.seq > watermark || envelope.event.is_live_only(),
        "durable event seq {} must be above the handshake watermark {watermark}",
        envelope.seq
    );
    if let Some(previous_seq) = previous_seq {
        assert!(
            envelope.seq >= previous_seq,
            "the forwarded stream must be cursor-ordered ({} after {previous_seq})",
            envelope.seq
        );
    }
}

/// N3 criterion 1: subscribe on a live host, drive real work through the
/// runtime, and receive typed runtime-event notifications on the same
/// connection — responses and notifications interleaved on one wire without
/// corrupting each other, including while the subscriber is completely idle.
async fn subscribe_delivers_runtime_events(endpoint: LocalEndpoint) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let run_id = fixture.composed.handle().run_id();
    let server = start_server(&fixture, endpoint, true).await?;

    let mut stream = connect(&server.endpoint).await;
    let mut notifications: Vec<PlatformEnvelope<WorkEventNotification>> = Vec::new();

    // Subscribe: the receipt carries the stream's starting watermark.
    let subscribe = request(
        "work",
        "subscribe",
        WorkSubscribeRequest {
            replay_after_seq: None,
        },
    );
    agent_host::write_frame(&mut stream, &serde_json::to_vec(&subscribe)?)?;
    let subscribed = expect_value(read_response_collecting::<WorkSubscribeResponse>(
        &mut stream,
        &subscribe.request_id,
        &mut notifications,
    )?);
    assert!(
        !subscribed.resync_required,
        "a fresh subscribe never needs resync"
    );
    let watermark = subscribed.watermark;

    // Drive real work: the submit journals runtime events, which must reach
    // this connection as typed notifications (possibly interleaved with the
    // submit receipt itself).
    let submit = request(
        "work",
        "submit",
        WorkSubmitRequest {
            goal: "host e2e: subscribe then submit".into(),
            client_request_id: "n3-events-1".into(),
        },
    );
    agent_host::write_frame(&mut stream, &serde_json::to_vec(&submit)?)?;
    let submitted = expect_value(read_response_collecting::<WorkSubmitResponse>(
        &mut stream,
        &submit.request_id,
        &mut notifications,
    )?);
    assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
    let _ = submitted.task_id;

    // Collect the first events with a bounded wait per frame, so a
    // regression that stops event delivery fails instead of hanging.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while notifications.len() < 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "expected runtime event notifications after submit"
        );
        match read_incoming_bounded(&mut stream, NOTIFICATION_BOUND)? {
            Some(Incoming::Notification(notification)) => notifications.push(*notification),
            Some(Incoming::Response { .. }) => {}
            None => continue,
        }
    }
    for (index, notification) in notifications.iter().enumerate() {
        assert_notification(
            notification,
            run_id,
            watermark,
            if index == 0 {
                None
            } else {
                Some(notifications[index - 1].payload.envelope.seq)
            },
        );
    }

    // Idle delivery: with this connection parked and sending nothing, a
    // second client drives fresh work (continue starts a new turn); the
    // subscriber must still receive the resulting events. This is the
    // regression guard for the named-pipe file-object rule: non-overlapped
    // I/O on one pipe instance serializes across handle duplicates, so a
    // request loop parked in a plain read would starve the forwarder.
    let before_idle = notifications.len();
    let probe_endpoint = server.endpoint.clone();
    let probe = std::thread::spawn(move || -> anyhow::Result<()> {
        let mut probe_client = connect_blocking(&probe_endpoint)?;
        let cont = request("work", "continue", WorkContinueRequest {});
        agent_host::write_frame(&mut probe_client, &serde_json::to_vec(&cont)?)?;
        let mut ignored = Vec::new();
        let response = read_response_collecting::<WorkContinueResponse>(
            &mut probe_client,
            &cont.request_id,
            &mut ignored,
        )?;
        expect_value(response);
        Ok(())
    });
    let idle_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while notifications.len() == before_idle {
        assert!(
            std::time::Instant::now() < idle_deadline,
            "an idle subscriber received no event within the deadline; \
             the forwarder is starved by the parked request loop"
        );
        match read_incoming_bounded(&mut stream, NOTIFICATION_BOUND)? {
            Some(Incoming::Notification(notification)) => notifications.push(*notification),
            Some(Incoming::Response { .. }) => {}
            None => continue,
        }
    }
    assert_notification(
        notifications.last().expect("idle notification collected"),
        run_id,
        watermark,
        None,
    );
    probe
        .join()
        .map_err(|_| anyhow::anyhow!("probe thread panicked"))??;

    // And the wire is still a working response plane after the event burst.
    let snapshot = request("work", "snapshot", WorkSnapshotRequest {});
    agent_host::write_frame(&mut stream, &serde_json::to_vec(&snapshot)?)?;
    let snapshot = expect_value(read_response_collecting::<WorkSnapshotResponse>(
        &mut stream,
        &snapshot.request_id,
        &mut notifications,
    )?);
    assert!(snapshot.run_started);
    assert!(
        snapshot.watermark > watermark,
        "the runtime's cursor must have advanced past the handshake watermark"
    );

    drop(stream);
    assert_registry_drained(&server.registry).await;
    fixture.composed.shutdown().await?;
    stop_and_join_bounded(server, BOUNDED_STOP).await?;
    Ok(())
}

/// N3 criterion 2: a subscriber that stops reading its socket must not
/// block other clients or the bounded stop. The host's queue for it is the
/// runtime's own bounded broadcast channel plus the kernel buffer — never
/// unbounded host memory — and the wind-down cancels its forwarder through
/// the same interrupt path as every other parked connection.
async fn slow_subscriber_never_blocks_service_or_stop(
    endpoint: LocalEndpoint,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;

    // The slow subscriber: completes the subscribe handshake, then never
    // reads another byte and never goes away.
    let mut slow = connect(&server.endpoint).await;
    let subscribe = request(
        "work",
        "subscribe",
        WorkSubscribeRequest {
            replay_after_seq: None,
        },
    );
    agent_host::write_frame(&mut slow, &serde_json::to_vec(&subscribe)?)?;
    let mut ignored = Vec::new();
    let subscribed: PlatformResponse<WorkSubscribeResponse> =
        read_response_collecting(&mut slow, &subscribe.request_id, &mut ignored)?;
    assert!(expect_value(subscribed).watermark > 0, "the run is live");
    assert!(ignored.is_empty(), "nothing else flows on a quiet run");

    // A healthy client is served while the silent subscriber exists.
    let mut healthy = connect(&server.endpoint).await;
    let before = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut healthy,
        &request("work", "snapshot", WorkSnapshotRequest {}),
    )?);
    let submitted = expect_value(exchange::<_, _, WorkSubmitResponse>(
        &mut healthy,
        &request(
            "work",
            "submit",
            WorkSubmitRequest {
                goal: "host e2e: events nobody reads".into(),
                client_request_id: "n3-slow-1".into(),
            },
        ),
    )?);
    assert_eq!(submitted.disposition, WorkSubmitDisposition::Accepted);
    // Real runtime events now flow into the slow subscriber's forwarder.
    // The healthy client keeps getting prompt, correlated answers.
    for _ in 0..3 {
        let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
            &mut healthy,
            &request("work", "snapshot", WorkSnapshotRequest {}),
        )?);
        assert!(snapshot.run_started);
    }
    let cancelled = expect_value(exchange::<_, _, WorkCancelResponse>(
        &mut healthy,
        &request("work", "cancel", WorkCancelRequest {}),
    )?);
    assert!(matches!(
        cancelled.ack,
        agent_contracts::TurnCancelAck::Cancelled { .. }
            | agent_contracts::TurnCancelAck::NoActiveTurn
    ));
    let after = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut healthy,
        &request("work", "snapshot", WorkSnapshotRequest {}),
    )?);
    assert!(
        after.watermark > before.watermark,
        "real runtime events must have been emitted for the forwarder to consume"
    );

    // The healthy client leaves; the silent subscriber stays parked across
    // the stop. Shutdown must remain bounded and every grant must revoke.
    drop(healthy);
    fixture.composed.shutdown().await?;
    let (elapsed, registry) = stop_and_join_bounded(server, BOUNDED_STOP).await?;
    eprintln!("stop with a silent subscriber took {elapsed:?}");
    assert_registry_drained(&registry).await;
    Ok(())
}

/// B2 RETYPED: a run-scoped request that arrives carrying a tool-operation
/// work identity gets the structured protocol rejection — the retype step
/// must hand the validator the envelope the client actually sent, not a
/// cleaned copy. The rejection is a paired response on the same connection,
/// which stays usable afterwards.
async fn run_scoped_request_with_work_identity_is_rejected(
    endpoint: LocalEndpoint,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let fixture = compose_workspace(dir.path()).await?;
    fixture.composed.instance.start().await?;
    let server = start_server(&fixture, endpoint, true).await?;

    let mut stream = connect(&server.endpoint).await;
    let mut carrying_work = request(
        "work",
        "submit",
        WorkSubmitRequest {
            goal: "host e2e: retyped envelope drill".into(),
            client_request_id: "e2e-retyped-1".into(),
        },
    );
    carrying_work.work = Some(agent_platform_protocol::WorkIdentity {
        run_id: agent_contracts::RunId::new(),
        task_id: None,
        turn_id: None,
        scope_id: None,
        operation_id: agent_contracts::OperationId::new(),
        generation: 0,
        attempt: agent_platform_protocol::Attempt::new(1).unwrap(),
        call_id: None,
        effect_id: None,
        argument_digest: agent_platform_protocol::ArgumentDigest::from_bytes([0x22; 32]),
        deadline_remaining_ms: agent_platform_protocol::DeadlineRemainingMs::new(30_000).unwrap(),
        authority_ref: None,
    });

    let rejected = exchange::<_, _, serde_json::Value>(&mut stream, &carrying_work)?;
    match rejected {
        PlatformResponse::Error { error } => {
            assert_eq!(
                error.class,
                agent_platform_protocol::PlatformErrorClass::Protocol
            );
            assert_eq!(error.code, "protocol.request_invalid");
            assert!(
                error.message.contains("run-scoped"),
                "the rejection must name the run-scoped work violation: {}",
                error.message
            );
        }
        PlatformResponse::Success { .. } => {
            panic!("a run-scoped request carrying a work identity must be rejected")
        }
    }

    // The structured rejection is not a teardown: the same connection keeps
    // serving legal requests (and the rejected goal was never admitted).
    let snapshot = expect_value(exchange::<_, _, WorkSnapshotResponse>(
        &mut stream,
        &request("work", "snapshot", WorkSnapshotRequest {}),
    )?);
    assert!(
        snapshot.focus.is_none(),
        "the rejected submission must not have created a focus"
    );

    drop(stream);
    fixture.composed.shutdown().await?;
    let (elapsed, registry) = stop_and_join_bounded(server, BOUNDED_STOP).await?;
    eprintln!("stop after the retyped drill took {elapsed:?}");
    assert_registry_drained(&registry).await;
    Ok(())
}

// multi_thread on purpose: the client side of this drill uses blocking
// std IO; on the default current_thread test runtime that would freeze the
// whole runtime and deadlock the server-side block_on calls.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_end_to_end_work_plane() {
    let name = format!("focus-agent-e2e-{}", uuid_like());
    run_e2e(LocalEndpoint::NamedPipe(name), "named-pipe")
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_end_to_end_work_plane() {
    let path = std::env::temp_dir().join(format!("focus-agent-e2e-{}.sock", uuid_like()));
    run_e2e(LocalEndpoint::UnixSocket(path), "uds")
        .await
        .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_serves_two_clients_concurrently() {
    two_clients_serve_independently(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-two-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_serves_two_clients_concurrently() {
    two_clients_serve_independently(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-two-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_grants_revoke_on_disconnect() {
    grants_revoke_on_disconnect(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-revoke-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_grants_revoke_on_disconnect() {
    grants_revoke_on_disconnect(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-revoke-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_stop_is_bounded_with_no_client() {
    stop_is_bounded_with_no_client(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-stop-idle-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_stop_is_bounded_with_no_client() {
    stop_is_bounded_with_no_client(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-stop-idle-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_stop_is_bounded_with_hostile_clients() {
    stop_is_bounded_with_hostile_clients(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-stop-hostile-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_stop_is_bounded_with_hostile_clients() {
    stop_is_bounded_with_hostile_clients(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-stop-hostile-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

/// A regular file squatting the endpoint path must refuse startup and stay
/// untouched — the host never deletes something it cannot prove it owns.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_refuses_a_regular_file_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("occupied.sock");
    std::fs::write(&path, b"precious user data").unwrap();
    let fixture = compose_workspace(dir.path()).await.unwrap();
    fixture.composed.instance.start().await.unwrap();
    let server = start_server(&fixture, LocalEndpoint::UnixSocket(path.clone()), false)
        .await
        .unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(server.serve.join());
    });
    let joined = done_rx
        .recv_timeout(BOUNDED_STOP)
        .expect("endpoint refusal must be immediate")
        .map_err(|_| anyhow::anyhow!("serve thread panicked"))
        .unwrap();
    let error = joined.expect_err("a regular file at the endpoint must refuse startup");
    assert!(
        error.to_string().contains("not a socket"),
        "unexpected refusal: {error}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"precious user data",
        "the occupying file must be left untouched"
    );
    fixture.composed.shutdown().await.unwrap();
}

/// A live listener on the endpoint must refuse takeover, keep serving, and
/// remove its own endpoint file on exit — never one it did not bind.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_refuses_a_live_listener_and_cleans_up_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.sock");
    let fixture = compose_workspace(dir.path()).await.unwrap();
    fixture.composed.instance.start().await.unwrap();
    let server = start_server(&fixture, LocalEndpoint::UnixSocket(path.clone()), true)
        .await
        .unwrap();

    let second = start_server(&fixture, LocalEndpoint::UnixSocket(path.clone()), false)
        .await
        .unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(second.serve.join());
    });
    let joined = done_rx
        .recv_timeout(BOUNDED_STOP)
        .expect("takeover refusal must be immediate")
        .map_err(|_| anyhow::anyhow!("serve thread panicked"))
        .unwrap();
    let error = joined.expect_err("a live listener must refuse takeover");
    assert!(
        error.to_string().contains("already served"),
        "unexpected refusal: {error}"
    );

    // The first host is unharmed and still serves a full exchange.
    let mut stream = connect(&server.endpoint).await;
    let snapshot = expect_value(
        exchange::<_, _, WorkSnapshotResponse>(
            &mut stream,
            &request("work", "snapshot", WorkSnapshotRequest {}),
        )
        .unwrap(),
    );
    assert!(snapshot.run_started);
    drop(stream);

    let (elapsed, _) = stop_and_join_bounded(server, BOUNDED_STOP).await.unwrap();
    eprintln!("uds bounded stop took {elapsed:?}");
    assert!(
        !path.exists(),
        "the host must remove its own endpoint file on exit"
    );
    fixture.composed.shutdown().await.unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_subscribe_delivers_runtime_events() {
    subscribe_delivers_runtime_events(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-events-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_subscribe_delivers_runtime_events() {
    subscribe_delivers_runtime_events(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-events-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_slow_subscriber_never_blocks_service_or_stop() {
    slow_subscriber_never_blocks_service_or_stop(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-slow-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_slow_subscriber_never_blocks_service_or_stop() {
    slow_subscriber_never_blocks_service_or_stop(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-slow-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn named_pipe_run_scoped_request_with_work_identity_is_rejected() {
    run_scoped_request_with_work_identity_is_rejected(LocalEndpoint::NamedPipe(format!(
        "focus-agent-e2e-retyped-{}",
        uuid_like()
    )))
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unix_socket_run_scoped_request_with_work_identity_is_rejected() {
    run_scoped_request_with_work_identity_is_rejected(LocalEndpoint::UnixSocket(
        std::env::temp_dir().join(format!("focus-agent-e2e-retyped-{}.sock", uuid_like())),
    ))
    .await
    .unwrap();
}

#[cfg(not(any(windows, unix)))]
compile_error!("the host e2e requires a local transport");

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // A per-process serial guards against endpoint-name collisions: CI
    // runners have shown coarse clock granularity, and this test binary
    // starts several hosts in parallel within one process — two tests
    // sampling the same clock tick would derive the SAME pipe/socket name,
    // and the second host's `FILE_FLAG_FIRST_PIPE_INSTANCE` (or `bind`)
    // then fails instantly while its probe waits out the whole connect
    // budget. The serial makes every call unique regardless of clock
    // resolution.
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{serial}-{}", std::process::id())
}

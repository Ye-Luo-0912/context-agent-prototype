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
use std::sync::atomic::{AtomicBool, Ordering};

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
    ProtocolVersion, RequestId, Route, WorkCancelRequest, WorkCancelResponse, WorkSnapshotRequest,
    WorkSnapshotResponse, WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse,
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
    // The serve thread composes a full workspace before creating the pipe;
    // on a loaded CI runner that setup alone can outlast several seconds,
    // so the budget is generous (30s) — this probes readiness, it measures
    // nothing about the stop bound.
    for _ in 0..150 {
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            return file;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("named pipe {path} never became connectable");
}

#[cfg(unix)]
async fn connect(endpoint: &LocalEndpoint) -> std::os::unix::net::UnixStream {
    let LocalEndpoint::UnixSocket(path) = endpoint else {
        unreachable!("unix test uses the UDS transport")
    };
    for _ in 0..150 {
        if let Ok(stream) = std::os::unix::net::UnixStream::connect(path) {
            return stream;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("uds socket {} never became connectable", path.display());
}
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
    if probe {
        // One throwaway connection proves the endpoint is bound before the
        // test proceeds; it is served and closed like any client.
        drop(connect(&endpoint).await);
    }
    Ok(TestServer {
        endpoint,
        stop,
        registry,
        serve,
    })
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

#[cfg(not(any(windows, unix)))]
compile_error!("the host e2e requires a local transport");

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}-{}", nanos, std::process::id())
}

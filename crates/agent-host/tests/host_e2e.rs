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

use agent_compose::{
    build_context_engine, compose, ComposeConfig, ContextPolicy, HostToolPolicyRegistry,
};
use agent_core::{ApprovalBroker, InteractiveApprovalGate, TaskApprovalGate};
use agent_host::{
    negotiated_profile, session_schema_digest, HostPlane, HostServer, LocalEndpoint,
    SingleInstance,
};
use agent_platform_protocol::{
    ApprovalRespondRequest, ApprovalRespondResponse, ApprovalRespondOutcome, Causality,
    ActiveFeatures, EnvelopeKind, MessageId, PlatformEnvelope, PlatformResponse, ProtocolIdentity,
    ProtocolVersion, RequestId, Route, WorkCancelRequest, WorkCancelResponse,
    WorkSnapshotRequest, WorkSnapshotResponse, WorkSubmitDisposition, WorkSubmitRequest,
    WorkSubmitResponse,
};
use agent_contracts::ApprovalGate as _;
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
    let lock = SingleInstance::acquire(&workspace.state_dir())?;
    let model: Arc<dyn agent_contracts::ModelTransport> =
        Arc::new(agent_compose::MockModelTransport);
    let context_engine =
        build_context_engine(ContextPolicy::Rolling, workspace.state_dir(), Some(model.clone()))
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
    let frame = agent_host::read_frame(stream)?
        .ok_or_else(|| anyhow::anyhow!("connection closed before response"))?;
    let envelope = serde_json::from_slice::<PlatformEnvelope<PlatformResponse<R>>>(&frame)?;
    assert_eq!(envelope.request_id, request.request_id, "correlated response");
    assert_eq!(envelope.kind, EnvelopeKind::Response);
    Ok(envelope.payload)
}

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

    let server = HostServer {
        endpoint: endpoint.clone(),
        read_only: false,
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
    let gate_decision = authorize.await.expect("authorize task").expect("gate result");
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
    let _ = serve_thread.join();
    eprintln!("e2e[{label}]: done");
    Ok(())
}

async fn connect(endpoint: &LocalEndpoint) -> std::fs::File {
    let LocalEndpoint::NamedPipe(name) = endpoint else {
        unreachable!("windows test uses the named pipe transport")
    };
    let path = format!(r"\\.\pipe\{name}");
    for _ in 0..50 {
        if let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            return file;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("named pipe {path} never became connectable");
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
    run_e2e(LocalEndpoint::UnixSocket(path), "uds").await.unwrap();
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

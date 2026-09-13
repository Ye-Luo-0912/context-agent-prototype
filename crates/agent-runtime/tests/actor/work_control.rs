//! P2 acceptance at the Platform application layer: the work-control router
//! serves the C0 work routes with typed receipts, one consistent snapshot,
//! honest subscribe/resync semantics, and session-bound approval responses.

use std::sync::Arc;

use agent_contracts::{ApprovalDecision, ToolCall, ToolRisk, ToolSpec};
use agent_contracts::{ApprovalGate, CancellationToken};
use agent_core::{ApprovalBroker, InteractiveApprovalGate};
use agent_platform_protocol::{
    ApprovalRespondOutcome, ApprovalRespondRequest, Causality, EnvelopeKind, MessageId,
    NegotiatedContractProfile, PlatformEnvelope, PlatformResponse, ProtocolIdentity,
    ProtocolVersion, RequestId, RetryDisposition, Route, SchemaDigest, WorkArtifactRequest,
    WorkChangesRequest, WorkSubmitDisposition, WorkSubmitRequest, WorkSubscribeRequest,
};
use agent_runtime::{
    WorkControlAction, WorkControlAuthorization, WorkControlAuthorizationRequest,
    WorkControlAuthorizer, WorkControlGrant, WorkControlRouter, WorkControlSessionRegistry,
};

use crate::harness::*;

fn protocol() -> ProtocolIdentity {
    ProtocolIdentity {
        name: "focus-agent.platform".into(),
        version: ProtocolVersion { major: 1, minor: 0 },
        active_features: Default::default(),
        schema_digest: SchemaDigest::from_bytes([0x11; 32]),
    }
}

fn profile() -> NegotiatedContractProfile {
    let identity = protocol();
    NegotiatedContractProfile::new(
        identity.name,
        identity.version,
        identity.active_features,
        identity.schema_digest,
    )
    .unwrap()
}

struct AllowAll;

impl WorkControlAuthorizer for AllowAll {
    fn authorize(&self, _request: &WorkControlAuthorizationRequest) -> WorkControlAuthorization {
        WorkControlAuthorization::Authorized
    }
}

struct DenySubmit;

impl WorkControlAuthorizer for DenySubmit {
    fn authorize(&self, request: &WorkControlAuthorizationRequest) -> WorkControlAuthorization {
        if request.action == WorkControlAction::Submit {
            WorkControlAuthorization::Denied
        } else {
            WorkControlAuthorization::Authorized
        }
    }
}

fn run_scoped_envelope<P>(route: Route, payload: P) -> PlatformEnvelope<P> {
    let message_id = MessageId::new();
    PlatformEnvelope {
        protocol: protocol(),
        message_id,
        request_id: Some(RequestId::new()),
        kind: EnvelopeKind::Request,
        route,
        work: None,
        causality: Causality::root(message_id),
        payload,
    }
}

async fn router_with(
    handle: agent_runtime::RuntimeHandle,
    authorizer: Arc<dyn WorkControlAuthorizer>,
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
) -> (WorkControlRouter, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = Arc::new(
        agent_workspace::Workspace::open(dir.path())
            .await
            .expect("open workspace"),
    );
    (
        WorkControlRouter::new(profile(), handle, broker, gate, authorizer, workspace).unwrap(),
        dir,
    )
}

#[tokio::test]
async fn submit_route_returns_receipts_and_dedups() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(
        handle.clone(),
        Arc::new(AllowAll),
        Arc::clone(&broker),
        Arc::clone(&gate),
    )
    .await;
    let mut events = handle.subscribe();

    let first = router
        .submit(run_scoped_envelope(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "router goal".into(),
                client_request_id: "req-1".into(),
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = first.payload else {
        panic!("expected success: {first:?}");
    };
    assert_eq!(value.disposition, WorkSubmitDisposition::Accepted);
    let first_task = value.task_id;
    wait_for_turn_completed(&mut events).await;

    // Identical retry: already accepted, same task.
    let retried = router
        .submit(run_scoped_envelope(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "router goal".into(),
                client_request_id: "req-1".into(),
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = retried.payload else {
        panic!("expected success: {retried:?}");
    };
    assert_eq!(value.disposition, WorkSubmitDisposition::AlreadyAccepted);
    assert_eq!(value.task_id, first_task);

    // Conflicting content under the same id: a typed rejection, never a
    // second execution.
    let conflict = router
        .submit(run_scoped_envelope(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "a different goal".into(),
                client_request_id: "req-1".into(),
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Error { error } = conflict.payload else {
        panic!("expected rejection: {conflict:?}");
    };
    assert_eq!(error.code, "work.rejected");
}

#[tokio::test]
async fn snapshot_is_consistent_and_subscribe_reports_stale_cursors() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(
        handle.clone(),
        Arc::new(AllowAll),
        Arc::clone(&broker),
        Arc::clone(&gate),
    )
    .await;
    let mut events = handle.subscribe();

    router
        .submit(run_scoped_envelope(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "snapshot goal".into(),
                client_request_id: "req-s".into(),
            },
        ))
        .await
        .unwrap();
    wait_for_turn_completed(&mut events).await;

    let snapshot = router
        .snapshot(run_scoped_envelope(
            Route::work_snapshot(),
            agent_platform_protocol::WorkSnapshotRequest {},
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = snapshot.payload else {
        panic!("expected success: {snapshot:?}");
    };
    assert!(value.run_started);
    let focus = value.focus.as_ref().expect("focused task");
    assert_eq!(focus.goal, "snapshot goal");
    assert_eq!(focus.task_id, value.tasks[0].task_id);
    assert_eq!(value.tasks[0].anchor_revision, focus.anchor_revision);

    // Subscribe from now: no resync needed.
    let (response, mut stream) = router
        .subscribe(run_scoped_envelope(
            Route::work_subscribe(),
            WorkSubscribeRequest::default(),
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = response.payload else {
        panic!("expected success: {response:?}");
    };
    assert!(!value.resync_required);
    assert!(value.watermark > 0);
    // Produce a new event after registration; waiting for an event on an
    // idle runtime would hang rather than exercise the live handoff.
    handle
        .start_work("live goal".into(), "live-id".into())
        .await
        .unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(next.seq > value.watermark);
    wait_for_turn_completed(&mut events).await;

    // A cursor older than the retained window is answered with the truth:
    // rebuild from a snapshot, the gap will not be filled.
    let (stale, _) = router
        .subscribe(run_scoped_envelope(
            Route::work_subscribe(),
            WorkSubscribeRequest {
                replay_after_seq: Some(value.watermark.saturating_sub(1)),
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = stale.payload else {
        panic!("expected success: {stale:?}");
    };
    assert!(value.resync_required);
}

#[tokio::test]
async fn denied_subscription_has_no_runtime_event_receiver() {
    struct DenySubscribe;
    impl WorkControlAuthorizer for DenySubscribe {
        fn authorize(&self, _: &WorkControlAuthorizationRequest) -> WorkControlAuthorization {
            WorkControlAuthorization::Denied
        }
    }
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(handle, Arc::new(DenySubscribe), broker, gate).await;
    let (response, mut stream) = router
        .subscribe(run_scoped_envelope(
            Route::work_subscribe(),
            WorkSubscribeRequest::default(),
        ))
        .await
        .unwrap();
    assert!(matches!(response.payload, PlatformResponse::Error { .. }));
    assert!(matches!(
        stream.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Closed)
    ));
}

#[tokio::test]
async fn subscribe_cannot_fabricate_a_watermark_after_actor_failure() {
    let (handle, task) = start(Arc::new(SilentModel)).await;
    task.abort();
    let _ = task.await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(handle, Arc::new(AllowAll), broker, gate).await;
    let (response, mut stream) = router
        .subscribe(run_scoped_envelope(
            Route::work_subscribe(),
            WorkSubscribeRequest::default(),
        ))
        .await
        .unwrap();
    assert!(matches!(response.payload, PlatformResponse::Error { .. }));
    assert!(matches!(
        stream.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Closed)
    ));
}

#[tokio::test]
async fn approval_respond_binds_to_the_pending_request_once() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(
        handle.clone(),
        Arc::new(AllowAll),
        Arc::clone(&broker),
        Arc::clone(&gate),
    )
    .await;

    // One authorization waiting on the approval plane (the kernel path in
    // production; here driven directly against the same gate the router
    // holds).
    let call = ToolCall {
        id: "call-1".into(),
        name: "fs.write".into(),
        arguments: serde_json::json!({"path": "a.txt"}),
    };
    let spec = ToolSpec {
        name: "fs.write".into(),
        description: "write".into(),
        input_schema: serde_json::json!({"type": "object"}),
        risk: ToolRisk::WorkspaceWrite,
        output_budget: None,
        roles: Vec::new(),
    };
    let cancel = CancellationToken::new();
    let authorize = {
        let gate = Arc::clone(&gate);
        let call = call.clone();
        let spec = spec.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { gate.authorize(&call, &spec, &cancel).await })
    };
    // Wait for the request to surface as pending.
    let mut pending_id = None;
    for _ in 0..50 {
        let pending = broker.pending().await;
        if let Some(request) = pending.first() {
            pending_id = Some(request.request_id.clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let request_id = pending_id.expect("a pending approval appeared");

    // The snapshot sees it.
    let snapshot = router
        .snapshot(run_scoped_envelope(
            Route::work_snapshot(),
            agent_platform_protocol::WorkSnapshotRequest {},
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = snapshot.payload else {
        panic!("expected success: {snapshot:?}");
    };
    assert_eq!(value.pending_approvals.len(), 1);
    assert_eq!(value.pending_approvals[0].request_id, request_id);
    assert_eq!(value.pending_approvals[0].call_name, "fs.write");

    // Deny it through the router: delivered once, then the fact.
    let deny = |request_id: String| {
        let router = &router;
        async move {
            router
                .respond(run_scoped_envelope(
                    Route::approval_respond(),
                    ApprovalRespondRequest {
                        request_id,
                        decision: ApprovalDecision::Deny,
                    },
                ))
                .await
                .unwrap()
        }
    };
    let first = deny(request_id.clone()).await;
    let PlatformResponse::Success { value } = first.payload else {
        panic!("expected success: {first:?}");
    };
    assert_eq!(value.outcome, ApprovalRespondOutcome::Delivered);

    let second = deny(request_id).await;
    let PlatformResponse::Success { value } = second.payload else {
        panic!("expected success: {second:?}");
    };
    assert_eq!(value.outcome, ApprovalRespondOutcome::NoLongerPending);

    let verdict = authorize.await.unwrap();
    assert!(
        matches!(verdict, Ok(ApprovalDecision::Deny)),
        "the deny must reach the waiting call: {verdict:?}"
    );
    let _ = cancel;
}

#[tokio::test]
async fn sessions_without_mutation_grants_cannot_submit() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, _dir) = router_with(handle.clone(), Arc::new(DenySubmit), broker, gate).await;

    let rejected = router
        .submit(run_scoped_envelope(
            Route::work_submit(),
            WorkSubmitRequest {
                goal: "nope".into(),
                client_request_id: "req-x".into(),
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Error { error } = rejected.payload else {
        panic!("expected rejection: {rejected:?}");
    };
    assert_eq!(error.code, "work.forbidden");
    assert!(matches!(error.retry, RetryDisposition::Never));
}

/// Session-registry semantics: installed grants authorize, revocation is
/// immediate, and unknown ids never pass.
#[tokio::test]
async fn session_registry_grants_and_revokes() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let registry = WorkControlSessionRegistry::new(handle.run_id());
    let session = registry.install(WorkControlGrant::operator()).unwrap();
    let bound = registry.bind(&session).unwrap();
    let request = WorkControlAuthorizationRequest {
        action: WorkControlAction::Submit,
        run_id: handle.run_id(),
        authority_ref: Some("peer-claims-anything".into()),
    };
    assert_eq!(
        bound.authorize(&request),
        WorkControlAuthorization::Authorized
    );
    registry.revoke(&session).unwrap();
    assert_eq!(
        bound.authorize(&request),
        WorkControlAuthorization::Denied,
        "revocation must take effect immediately"
    );
    assert!(registry.bind(&session).is_err());
}

/// PLATFORM-2: a journaled change whose before-body was captured carries a
/// run-scoped artifact reference on the wire, and the paged artifact route
/// reads the original bytes back. Content itself still never travels in the
/// change row.
#[tokio::test]
async fn changes_rows_locate_captured_content_through_an_artifact_reference() {
    let (handle, _task) = start(Arc::new(SilentModel)).await;
    let broker = ApprovalBroker::new();
    let gate = Arc::new(InteractiveApprovalGate::new(Arc::clone(&broker)));
    let (router, dir) = router_with(
        handle.clone(),
        Arc::new(AllowAll),
        Arc::clone(&broker),
        Arc::clone(&gate),
    )
    .await;

    // Journal one prepared mutation with a small before-body. Appending the
    // record directly keeps the router's workspace the only instance (its
    // effect journal holds an exclusive lock); `read_changes` reads exactly
    // this JSONL surface.
    let state_dir = dir.path().join(".focus-agent");
    std::fs::create_dir_all(&state_dir).unwrap();
    let record = agent_workspace::ChangeRecord::MutationPrepared {
        tx_id: "tx-locate-1".into(),
        timestamp_ms: 1,
        tool: "fs.write".into(),
        path: "notes.txt".into(),
        action: "overwrite".into(),
        bytes_before: 18,
        bytes_after: 8,
        before_hash: "0".repeat(16),
        after_hash: "1".repeat(16),
        old_content: Some("old body to locate".into()),
    };
    use std::io::Write;
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("changes.jsonl"))
        .unwrap();
    writeln!(journal, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    journal.flush().unwrap();

    let listed = router
        .changes(run_scoped_envelope(
            Route::work_changes(),
            WorkChangesRequest {
                limit: None,
                after_tx: None,
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = listed.payload else {
        panic!("expected success: {listed:?}");
    };
    let prepared = value
        .changes
        .iter()
        .find_map(|row| match row {
            agent_platform_protocol::ChangeSummary::MutationPrepared {
                tx_id,
                old_content_artifact,
                ..
            } => Some((tx_id.clone(), old_content_artifact.clone())),
            _ => None,
        })
        .expect("the prepared mutation must be listed");
    let reference = prepared
        .1
        .clone()
        .expect("a captured before-body must expose an artifact reference");

    // The reference resolves through the paged artifact route to the exact
    // original bytes.
    let page = router
        .artifact(run_scoped_envelope(
            Route::work_artifact(),
            WorkArtifactRequest {
                reference,
                max_bytes: None,
                offset: None,
            },
        ))
        .await
        .unwrap();
    let PlatformResponse::Success { value } = page.payload else {
        panic!("expected success: {page:?}");
    };
    assert!(!value.truncated, "a small body fits one page");
    assert!(value.next_offset.is_none());
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&value.content_base64)
        .unwrap();
    assert_eq!(decoded, b"old body to locate");
    let _ = prepared.0;
}

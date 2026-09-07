//! C0 cross-language contract fixtures.
//!
//! These files under `tests/fixtures/work/` are the single source of truth
//! shared with the .NET client suite (`clients/dotnet/Agent.Client.Tests`).
//! Each fixture is decoded into the typed Rust DTO, validated through the
//! crate's route validators, re-encoded, and compared byte-for-byte — the
//! same three steps the C# suite performs, so a shape drift fails on both
//! sides instead of one language silently agreeing with itself.

use std::path::PathBuf;

use agent_platform_protocol::{
    ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse, Attempt,
    DeadlineRemainingMs, EnvelopeKind, MessageId, NegotiatedContractProfile, PlatformEnvelope,
    PlatformResponse, ProtocolIdentity, ProtocolVersion, RequestId, SchemaDigest,
    TaskSnapshotStatus, WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse,
    validate_approval_respond_request, validate_approval_respond_response,
    validate_work_cancel_request, validate_work_cancel_response, validate_work_continue_request,
    validate_work_continue_response, validate_work_snapshot_request,
    validate_work_snapshot_response, validate_work_submit_request, validate_work_submit_response,
    validate_work_subscribe_request, validate_work_subscribe_response,
};
use std::str::FromStr;

use agent_contracts::{ApprovalDecision, TurnCancelAck};
use serde::de::DeserializeOwned;

/// The route validator shape shared by every request/response fixture pair.
type FixtureValidator<TRequest, TResponse> = fn(
    &NegotiatedContractProfile,
    &PlatformEnvelope<TRequest>,
    &PlatformEnvelope<PlatformResponse<TResponse>>,
) -> agent_platform_protocol::ValidationResult<()>;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/work")
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixtures_dir().join(name))
        .unwrap_or_else(|error| panic!("fixture {name} must exist: {error}"))
}

fn profile() -> NegotiatedContractProfile {
    let identity = ProtocolIdentity {
        name: "focus-agent.platform".into(),
        version: ProtocolVersion { major: 1, minor: 0 },
        active_features: Default::default(),
        schema_digest: SchemaDigest::from_bytes([0x11; 32]),
    };
    NegotiatedContractProfile::new(
        identity.name,
        identity.version,
        identity.active_features,
        identity.schema_digest,
    )
    .unwrap()
}

/// Re-encoding must be byte-identical: field order, skipping, and enum
/// casing are all wire contract, not implementation detail.
fn assert_round_trip_is_byte_identical<T>(text: &str, name: &str)
where
    T: DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let decoded: T = serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("fixture {name} must decode: {error}"));
    let encoded = serde_json::to_string(&decoded)
        .unwrap_or_else(|error| panic!("fixture {name} must encode: {error}"));
    assert_eq!(
        encoded,
        text.trim_end_matches('\n'),
        "fixture {name} must round-trip byte-identically"
    );
}

fn request_pair<TRequest, TResponse>(
    request_name: &str,
    response_name: &str,
    validate_request: fn(
        &NegotiatedContractProfile,
        &PlatformEnvelope<TRequest>,
    ) -> agent_platform_protocol::ValidationResult<()>,
    validate_response: FixtureValidator<TRequest, TResponse>,
) -> (PlatformEnvelope<TRequest>, TResponse)
where
    TRequest: DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
    TResponse: DeserializeOwned + serde::Serialize + PartialEq + std::fmt::Debug,
{
    let request_text = read_fixture(request_name);
    let response_text = read_fixture(response_name);
    assert_round_trip_is_byte_identical::<PlatformEnvelope<TRequest>>(&request_text, request_name);
    assert_round_trip_is_byte_identical::<PlatformEnvelope<PlatformResponse<TResponse>>>(
        &response_text,
        response_name,
    );

    let request: PlatformEnvelope<TRequest> = serde_json::from_str(&request_text).unwrap();
    let response: PlatformEnvelope<PlatformResponse<TResponse>> =
        serde_json::from_str(&response_text).unwrap();
    validate_request(&profile(), &request)
        .unwrap_or_else(|error| panic!("{request_name} must validate: {error}"));
    validate_response(&profile(), &request, &response)
        .unwrap_or_else(|error| panic!("{response_name} must validate: {error}"));

    let PlatformResponse::Success { value } = response.payload else {
        panic!("{response_name} fixture must be a success response");
    };
    (request, value)
}

#[test]
fn submit_fixture_pair_means_acceptance_with_task_binding() {
    let (request, value) = request_pair(
        "submit_request.json",
        "submit_response.json",
        validate_work_submit_request,
        validate_work_submit_response,
    );
    assert_eq!(request.payload.goal, "migrate the retry table");
    assert_eq!(request.payload.client_request_id, "client-1");
    assert_eq!(value.disposition, WorkSubmitDisposition::Accepted);
    assert!(!value.task_id.to_string().is_empty());
}

#[test]
fn error_fixture_carries_the_structured_conflict_fact() {
    let request_text = read_fixture("submit_request.json");
    let response_text = read_fixture("error_response.json");
    let request: PlatformEnvelope<WorkSubmitRequest> = serde_json::from_str(&request_text).unwrap();
    let response: PlatformEnvelope<PlatformResponse<WorkSubmitResponse>> =
        serde_json::from_str(&response_text).unwrap();
    validate_work_submit_response(&profile(), &request, &response).unwrap();
    let PlatformResponse::Error { error } = response.payload else {
        panic!("error_response.json must carry an error body");
    };
    assert_eq!(error.code, "work.goal_conflict");
    assert_eq!(
        error.retry,
        agent_platform_protocol::RetryDisposition::Never
    );
    error.validate().unwrap();
}

#[test]
fn continue_fixture_pair_binds_the_active_task() {
    let (_, value) = request_pair(
        "continue_request.json",
        "continue_response.json",
        validate_work_continue_request,
        validate_work_continue_response,
    );
    assert!(!value.task_id.to_string().is_empty());
}

#[test]
fn cancel_fixture_pair_reports_core_truth_not_optimism() {
    let (_, value) = request_pair(
        "cancel_request.json",
        "cancel_response.json",
        validate_work_cancel_request,
        validate_work_cancel_response,
    );
    let TurnCancelAck::Cancelled {
        task_id,
        cancelled_generation,
        effective_generation,
        ..
    } = value.ack
    else {
        panic!("cancel fixture must use the cancelled variant");
    };
    assert_eq!(cancelled_generation, 3);
    assert!(effective_generation >= cancelled_generation);
    assert_eq!(task_id.map(|id| id.to_string()), Some(value_task_id()));
}

fn value_task_id() -> String {
    "00000000-0000-4000-8000-000000000022".into()
}

#[test]
fn snapshot_fixture_pair_is_bounded_typed_and_watermarked() {
    let (_, value) = request_pair(
        "snapshot_request.json",
        "snapshot_response.json",
        validate_work_snapshot_request,
        validate_work_snapshot_response,
    );
    assert!(value.run_started);
    assert!(!value.run_completed);
    assert_eq!(value.watermark, 41);
    let focus = value.focus.expect("fixture has a focused task");
    assert_eq!(focus.anchor_revision, 1);
    assert_eq!(value.tasks.len(), 1);
    assert_eq!(value.tasks[0].status, TaskSnapshotStatus::Active);
    assert_eq!(value.pending_approvals.len(), 1);
    assert_eq!(value.pending_approvals[0].call_name, "fs.write");
    assert!(!value.resync_required);
}

#[test]
fn subscribe_fixture_pair_starts_the_stream_at_the_watermark() {
    let (request, value) = request_pair(
        "subscribe_request.json",
        "subscribe_response.json",
        validate_work_subscribe_request,
        validate_work_subscribe_response,
    );
    assert_eq!(request.payload.replay_after_seq, Some(17));
    assert_eq!(value.watermark, 41);
    assert!(!value.resync_required);
}

#[test]
fn approval_fixture_decision_is_the_exact_serbe_variant_name() {
    let request_text = read_fixture("approval_respond_request.json");
    // ApprovalDecision has no serde rename: the wire value is verbatim
    // "Allow"/"Deny". A rename on either side breaks this fixture first.
    assert!(request_text.contains("\"decision\":\"Allow\""));

    let (request, value) = request_pair(
        "approval_respond_request.json",
        "approval_respond_response.json",
        validate_approval_respond_request,
        validate_approval_respond_response,
    );
    assert_eq!(request.payload.decision, ApprovalDecision::Allow);
    assert_eq!(value.outcome, ApprovalRespondOutcome::Delivered);

    let deny: PlatformEnvelope<ApprovalRespondRequest> =
        serde_json::from_str(&request_text.replace("\"Allow\"", "\"Deny\"")).unwrap();
    assert_eq!(deny.payload.decision, ApprovalDecision::Deny);
    validate_approval_respond_request(&profile(), &deny).unwrap();

    let no_longer_pending = read_fixture("approval_respond_response.json")
        .replace("\"delivered\"", "\"no_longer_pending\"");
    let response: PlatformEnvelope<PlatformResponse<ApprovalRespondResponse>> =
        serde_json::from_str(&no_longer_pending).unwrap();
    let PlatformResponse::Success { value } = response.payload else {
        panic!("approval response fixture must succeed");
    };
    assert_eq!(value.outcome, ApprovalRespondOutcome::NoLongerPending);
}

#[test]
fn fixtures_reject_drifted_envelopes() {
    let profile = profile();
    let request_text = read_fixture("submit_request.json");

    // A run-scoped envelope must not start carrying a tool-operation work
    // identity on either side of the wire.
    let mut drifted: serde_json::Value = serde_json::from_str(&request_text).unwrap();
    drifted["work"] = serde_json::json!({
        "run_id": "00000000-0000-4000-8000-000000000021",
        "operation_id": "00000000-0000-4000-8000-000000000025",
        "generation": 1,
        "attempt": 1,
        "argument_digest": "22".repeat(32),
        "deadline_remaining_ms": 1_000,
    });
    let decoded: PlatformEnvelope<WorkSubmitRequest> =
        serde_json::from_value(drifted).expect("the injected work field itself is well-formed");
    let error = validate_work_submit_request(&profile, &decoded)
        .expect_err("work identity on run-scoped envelope must fail validation");
    assert_eq!(error.field(), "envelope.work");

    // Unknown route operations are outside the negotiated set.
    let mut unknown_route: serde_json::Value = serde_json::from_str(&request_text).unwrap();
    unknown_route["route"]["operation"] = serde_json::json!("archive");
    let decoded: PlatformEnvelope<WorkSubmitRequest> =
        serde_json::from_value(unknown_route).expect("route strings alone decode fine");
    assert!(
        validate_work_submit_request(&profile, &decoded).is_err(),
        "an unknown route operation must not pass validation"
    );
}

/// Guards the envelope primitives the fixtures rely on, so a silent change
/// to id/attempt/deadline validation shows up here and not only in prod.
#[test]
fn envelope_primitives_stay_fail_closed() {
    assert!(MessageId::from_str("00000000-0000-0000-0000-000000000000").is_err());
    assert!(MessageId::from_str("00000000-0000-4000-8000-000000000001").is_ok());
    assert!(Attempt::new(0).is_err());
    assert!(DeadlineRemainingMs::new(u32::MAX).is_err());
    assert_eq!(RequestId::new().to_string().len(), 36);
    assert_eq!(
        serde_json::to_string(&EnvelopeKind::Notification).unwrap(),
        "\"notification\""
    );
}

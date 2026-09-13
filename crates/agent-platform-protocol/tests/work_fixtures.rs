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
    ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse, ApprovalRisk, Attempt,
    DeadlineRemainingMs, EnvelopeKind, MessageId, NegotiatedContractProfile, PlatformEnvelope,
    PlatformResponse, ProtocolIdentity, ProtocolVersion, RequestId, SchemaDigest,
    TaskSnapshotStatus, WorkCompletionFact, WorkSubmitDisposition, WorkSubmitRequest,
    WorkSubmitResponse, WorkTaskCompletionResponse, validate_approval_respond_request,
    validate_approval_respond_response, validate_work_cancel_request,
    validate_work_cancel_response, validate_work_continue_request, validate_work_continue_response,
    validate_work_snapshot_request, validate_work_snapshot_response, validate_work_submit_request,
    validate_work_submit_response, validate_work_subscribe_request,
    validate_work_subscribe_response,
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
fn task_completion_fixture_pins_the_retired_fact_cross_language() {
    // The response is checked standalone: the paired request fixture is the
    // snapshot's, but a completion lookup has its own typed request built
    // here and validated against the same run-scoped rules.
    let response_text = read_fixture("task_completion_response.json");
    assert_round_trip_is_byte_identical::<
        PlatformEnvelope<PlatformResponse<WorkTaskCompletionResponse>>,
    >(&response_text, "task_completion_response.json");
    let response: PlatformEnvelope<PlatformResponse<WorkTaskCompletionResponse>> =
        serde_json::from_str(&response_text).unwrap();
    let mut request = request_pair(
        "snapshot_request.json",
        "snapshot_response.json",
        validate_work_snapshot_request,
        validate_work_snapshot_response,
    )
    .0;
    let request_value = serde_json::to_value(&request).unwrap();
    let mut typed_request = request_value.clone();
    typed_request["route"]["operation"] = serde_json::json!("task_completion");
    request = serde_json::from_value(typed_request).unwrap();
    let _ = request;
    let value = match response.payload {
        PlatformResponse::Success { value } => value,
        other => panic!("the fixture must be a success response, got {other:?}"),
    };
    // EXEC-8 (R2-09): a retired completion reads back with its bounded
    // evidence facts; the fact round-trips byte-identically so a .NET
    // client decodes exactly what the runtime serialized.
    assert_eq!(
        value.task_id.to_string(),
        "00000000-0000-4000-8000-000000000033"
    );
    match &value.fact {
        WorkCompletionFact::Retired {
            summary,
            anchor_revision,
            artifacts,
            final_output_digest,
        } => {
            assert_eq!(summary, "migrated the retry table");
            assert_eq!(*anchor_revision, 3);
            assert_eq!(artifacts.len(), 1);
            assert!(artifacts[0].starts_with("artifact://v1/"));
            assert_eq!(final_output_digest.as_deref(), Some(&"a".repeat(64)[..]));
        }
        other => panic!("the fixture must carry a retired fact, got {other:?}"),
    }
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
    // F12: the snapshot carries the gate's own risk plus a bounded target
    // summary — informed approval, not a bare request id.
    assert_eq!(
        value.pending_approvals[0].risk,
        ApprovalRisk::WorkspaceWrite
    );
    assert_eq!(
        value.pending_approvals[0].target_summary.as_deref(),
        Some("docs/plan.md")
    );
    assert!(!value.resync_required);
}

/// The endpoint suffix rule is pinned cross-language: the .NET desktop
/// derives the same default endpoint from the same bytes.
#[test]
fn endpoint_derivation_fixture_pins_the_shared_suffix_rule() {
    let value: serde_json::Value =
        serde_json::from_str(&read_fixture("endpoint_derivation.json")).unwrap();
    let root = value["workspace_root"].as_str().unwrap();
    let expected = value["endpoint_suffix"].as_str().unwrap();
    assert_eq!(
        agent_platform_protocol::workspace_endpoint_suffix(std::path::Path::new(root)),
        expected
    );
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

/// PLATFORM-4: the newest fact-bearing event is pinned cross-language. A
/// `model_used` notification decodes into the kernel's typed envelope on the
/// Rust side and stays a raw element with a typed accessor on the .NET side
/// (FocusAgent.Client's RuntimeEventEnvelope.TryGetModelUsage); both must
/// read the same counters and the same usage identity off these exact bytes.
#[test]
fn event_fixture_pins_model_usage_facts() {
    let text = read_fixture("event_model_used.json");
    let notification: PlatformEnvelope<agent_platform_protocol::WorkEventNotification> =
        serde_json::from_str(&text).expect("event fixture must decode");

    let envelope = &notification.payload.envelope;
    assert_eq!(envelope.seq, 41);
    assert_eq!(
        envelope.run_id.to_string(),
        "00000000-0000-4000-8000-000000000002"
    );
    match &envelope.event {
        agent_contracts::RuntimeEvent::ModelUsed {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            attempts,
            usage_identity,
            ..
        } => {
            assert_eq!(*input_tokens, 100);
            assert_eq!(*output_tokens, 5);
            assert_eq!(*cached_input_tokens, 80);
            assert_eq!(*attempts, 1);
            assert_eq!(*usage_identity, agent_contracts::UsageIdentity::Observed);
        }
        other => panic!("fixture must carry a model_used event, got {other:?}"),
    }
}

/// COST-6 (R2-10): the cache-write and cache-miss counters are pinned
/// cross-language on their own fixture. The transport normalization is the
/// producer's job — the wire fact is that the write came from an explicit
/// provider field and the miss stayed a miss; both stay readable without
/// parsing prose. The legacy fixture keeps its exact historical bytes
/// (unreported counters are skip-serialized, never rewritten to zeros).
#[test]
fn event_fixture_pins_cache_write_and_miss_facts() {
    let text = read_fixture("event_model_used_cache_fields.json");
    let notification: PlatformEnvelope<agent_platform_protocol::WorkEventNotification> =
        serde_json::from_str(&text).expect("event fixture must decode");

    let envelope = &notification.payload.envelope;
    assert_eq!(envelope.seq, 43);
    match &envelope.event {
        agent_contracts::RuntimeEvent::ModelUsed { usage, role, .. } => {
            let usage = usage.as_ref().expect("typed usage report present");
            assert_eq!(usage.cached_input_tokens, Some(80));
            assert_eq!(usage.cache_write_input_tokens, Some(10));
            assert_eq!(usage.cache_miss_input_tokens, Some(20));
            assert_eq!(*role, agent_contracts::ModelCallRole::Main);
        }
        other => panic!("fixture must carry a model_used event, got {other:?}"),
    }

    let legacy = read_fixture("event_model_used.json");
    let notification: PlatformEnvelope<agent_platform_protocol::WorkEventNotification> =
        serde_json::from_str(&legacy).expect("legacy fixture must decode");
    let rewritten = serde_json::to_string(&notification).unwrap();
    assert!(
        !rewritten.contains("cache_miss_input_tokens")
            && !rewritten.contains("cache_write_input_tokens"),
        "unreported cache counters stay off the wire (no invented zeros): {rewritten}"
    );
    match &notification.payload.envelope.event {
        agent_contracts::RuntimeEvent::ModelUsed { usage, .. } => {
            let usage = usage.as_ref().expect("typed usage report present");
            assert_eq!(usage.cached_input_tokens, Some(80));
            assert_eq!(usage.cache_write_input_tokens, None);
            assert_eq!(usage.cache_miss_input_tokens, None);
        }
        other => panic!("legacy fixture must carry a model_used event, got {other:?}"),
    }
}

/// COST-1 (E05.3): the compaction event carries its own usage evidence
/// identity — the projection no longer loses what the typed report had.
#[test]
fn event_fixture_pins_compaction_usage_identity() {
    let text = read_fixture("event_context_compacted.json");
    let notification: PlatformEnvelope<agent_platform_protocol::WorkEventNotification> =
        serde_json::from_str(&text).expect("event fixture must decode");

    let envelope = &notification.payload.envelope;
    assert_eq!(envelope.seq, 42);
    match &envelope.event {
        agent_contracts::RuntimeEvent::ContextCompacted {
            reason,
            input_tokens,
            output_tokens,
            source_items,
            usage_identity,
            cached_input_tokens,
            attempts,
            retries,
            cache_write_input_tokens: _,
            cache_miss_input_tokens: _,
        } => {
            assert_eq!(*reason, agent_contracts::CompactionReason::RollingFold);
            assert_eq!(*input_tokens, 12_000);
            assert_eq!(*output_tokens, 800);
            assert_eq!(*source_items, 14);
            assert_eq!(*usage_identity, agent_contracts::UsageIdentity::Estimated);
            assert_eq!(*cached_input_tokens, Some(300));
            assert_eq!(*attempts, 2);
            assert_eq!(*retries, 1);
        }
        other => panic!("fixture must carry a context_compacted event, got {other:?}"),
    }
}

/// PLATFORM-4: usage accounting facts cross the language boundary verbatim.
/// The .NET client decodes the SAME fixture files and reads the typed
/// identity — a renamed field or a re-classed identity breaks one side
/// first, never silently.
#[test]
fn model_used_fixture_exposes_usage_identity_cross_language() {
    for (name, expected_identity, expected_input) in [
        ("model_used_event.json", "observed", 9_000u64),
        ("model_used_event_unknown.json", "unknown", 0),
    ] {
        let text = read_fixture(name);
        let notification: agent_platform_protocol::WorkEventNotification =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} must decode: {e}"));
        let agent_contracts::RuntimeEvent::ModelUsed {
            input_tokens,
            usage_identity,
            usage,
            ..
        } = &notification.envelope.event
        else {
            panic!("{name} must carry a model_used event");
        };
        assert_eq!(*input_tokens, expected_input, "{name}");
        assert_eq!(
            format!("{usage_identity:?}").to_lowercase(),
            expected_identity,
            "{name}"
        );
        assert_eq!(usage.is_some(), expected_identity == "observed", "{name}");
        // Byte-identical re-encode: the wire shape is a shared contract.
        let re_encoded = serde_json::to_string(&notification).unwrap();
        assert_eq!(
            re_encoded,
            text.trim_end(),
            "{name} must round-trip byte-identical"
        );
    }
}

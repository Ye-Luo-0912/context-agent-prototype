//! Typed, transport-independent operation-control routes.
//!
//! Logical identity stays in [`WorkIdentity`]. Query and cancellation bodies
//! are therefore deliberately empty: copying identity into a second location
//! would create an ambiguity a peer could exploit. Responses preserve Core's
//! bounded [`OperationQueryResult`] truth instead of translating it into an
//! optimistic boolean acknowledgement.

use agent_contracts::{OperationQueryResult, OperationSnapshot};
use serde::{Deserialize, Serialize};

use crate::{
    EnvelopeKind, NegotiatedContractProfile, PlatformEnvelope, PlatformResponse, Route,
    ValidationError, ValidationResult, WorkIdentity, validate_response_pair,
};

pub const OPERATION_NAMESPACE: &str = "operation";
pub const OPERATION_QUERY: &str = "query";
pub const OPERATION_CANCEL: &str = "cancel";

impl Route {
    pub fn operation_query() -> Self {
        Self {
            namespace: OPERATION_NAMESPACE.to_owned(),
            operation: OPERATION_QUERY.to_owned(),
        }
    }

    pub fn operation_cancel() -> Self {
        Self {
            namespace: OPERATION_NAMESPACE.to_owned(),
            operation: OPERATION_CANCEL.to_owned(),
        }
    }

    pub fn is_operation_query(&self) -> bool {
        self.namespace == OPERATION_NAMESPACE && self.operation == OPERATION_QUERY
    }

    pub fn is_operation_cancel(&self) -> bool {
        self.namespace == OPERATION_NAMESPACE && self.operation == OPERATION_CANCEL
    }
}

/// The complete query body. The target identity is `envelope.work`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationQueryRequest {}

impl OperationQueryRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

/// The complete cancellation body. The target identity is `envelope.work`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCancelRequest {}

impl OperationCancelRequest {
    pub const fn validate(&self) -> ValidationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationQueryResponse {
    pub result: OperationQueryResult,
}

impl OperationQueryResponse {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_query_result(&self.result)
    }

    pub fn validate_against_work(&self, work: &WorkIdentity) -> ValidationResult<()> {
        self.validate()?;
        validate_result_identity(&self.result, work)
    }
}

/// Cancellation acknowledgement carrying Core's exact post-attempt truth.
/// In particular, `CommitStarted` and an applied terminal remain those states;
/// this DTO never relabels them as cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationCancelAck {
    pub result: OperationQueryResult,
}

impl OperationCancelAck {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_query_result(&self.result)
    }

    pub fn validate_against_work(&self, work: &WorkIdentity) -> ValidationResult<()> {
        self.validate()?;
        validate_result_identity(&self.result, work)
    }
}

/// Whether an admission response created a new reservation or found the same
/// logical operation already recorded. Both cases include the authoritative
/// state so a duplicate can never be mistaken for permission to redispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionDisposition {
    Accepted,
    AlreadyKnown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationAccepted {
    pub disposition: AdmissionDisposition,
    pub result: OperationQueryResult,
}

impl OperationAccepted {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_query_result(&self.result)?;
        if !matches!(self.result, OperationQueryResult::Found { .. }) {
            return Err(ValidationError::new(
                "operation_accepted.result",
                "accepted/already-known admission must carry a found operation snapshot",
            ));
        }
        Ok(())
    }

    pub fn validate_against_work(&self, work: &WorkIdentity) -> ValidationResult<()> {
        self.validate()?;
        validate_result_identity(&self.result, work)
    }
}

pub fn validate_operation_query_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<OperationQueryRequest>,
) -> ValidationResult<()> {
    validate_operation_request(profile, request, Route::is_operation_query)?;
    request.payload.validate()
}

pub fn validate_operation_cancel_request(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<OperationCancelRequest>,
) -> ValidationResult<()> {
    validate_operation_request(profile, request, Route::is_operation_cancel)?;
    request.payload.validate()
}

pub fn validate_operation_query_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<OperationQueryRequest>,
    response: &PlatformEnvelope<PlatformResponse<OperationQueryResponse>>,
) -> ValidationResult<()> {
    validate_operation_query_request(profile, request)?;
    validate_response_pair(profile, request, response)?;
    if let PlatformResponse::Success { value } = &response.payload {
        value.validate_against_work(
            response
                .work
                .as_ref()
                .expect("generic response validation requires work"),
        )?;
    }
    Ok(())
}

pub fn validate_operation_cancel_response(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<OperationCancelRequest>,
    response: &PlatformEnvelope<PlatformResponse<OperationCancelAck>>,
) -> ValidationResult<()> {
    validate_operation_cancel_request(profile, request)?;
    validate_response_pair(profile, request, response)?;
    if let PlatformResponse::Success { value } = &response.payload {
        value.validate_against_work(
            response
                .work
                .as_ref()
                .expect("generic response validation requires work"),
        )?;
    }
    Ok(())
}

fn validate_operation_request<P>(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<P>,
    route_matches: fn(&Route) -> bool,
) -> ValidationResult<()> {
    request.validate(profile)?;
    if request.kind != EnvelopeKind::Request {
        return Err(ValidationError::new(
            "envelope.kind",
            "operation control requires a request envelope",
        ));
    }
    if !route_matches(&request.route) {
        return Err(ValidationError::new(
            "envelope.route",
            "does not match the operation-control payload",
        ));
    }
    let work = request
        .work
        .as_ref()
        .expect("generic request validation requires work");
    if work.generation == 0 {
        return Err(ValidationError::new("work.generation", "must be non-zero"));
    }
    if work.turn_id.is_none() || work.call_id.is_none() {
        return Err(ValidationError::new(
            "work.identity",
            "tool operation control requires turn_id and call_id",
        ));
    }
    if work.deadline_remaining_ms.is_exhausted() {
        return Err(ValidationError::new(
            "work.deadline_remaining_ms",
            "is exhausted; the operation-control request must not be dispatched",
        ));
    }
    if work.effect_id.is_some() {
        return Err(ValidationError::new(
            "work.effect_id",
            "operation query/cancel requests target the logical operation; effect truth is returned by Core and must not be supplied as a qualifier",
        ));
    }
    Ok(())
}

fn validate_query_result(result: &OperationQueryResult) -> ValidationResult<()> {
    result
        .validate()
        .map_err(|reason| ValidationError::new("operation.result", reason))
}

fn validate_result_identity(
    result: &OperationQueryResult,
    work: &WorkIdentity,
) -> ValidationResult<()> {
    let OperationQueryResult::Found { snapshot } = result else {
        return Ok(());
    };
    validate_snapshot_identity(snapshot, work)
}

fn validate_snapshot_identity(
    snapshot: &OperationSnapshot,
    work: &WorkIdentity,
) -> ValidationResult<()> {
    let identity = &snapshot.identity;
    let matches = identity.run_id == work.run_id
        && identity.task_id == work.task_id
        && Some(identity.turn_id) == work.turn_id
        && identity.scope_id == work.scope_id
        && identity.operation_id == work.operation_id
        && identity.generation == work.generation
        && work.call_id.as_deref() == Some(identity.call_id.as_str())
        && identity.argument_digest == work.argument_digest;
    if !matches {
        return Err(ValidationError::new(
            "operation.result.identity",
            "found snapshot does not match envelope work identity",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use agent_contracts::{
        ArgumentDigest, EffectDurability, EffectId, OperationId, OperationSnapshot, OperationState,
        OperationTerminal, RunId, ScopeId, TaskId, ToolOperationIdentity, TurnId,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        ActiveFeatures, Attempt, Causality, DeadlineRemainingMs, MessageId, ProtocolIdentity,
        ProtocolVersion, RequestId, SchemaDigest,
    };

    const MESSAGE_1: &str = "00000000-0000-4000-8000-000000000001";
    const MESSAGE_2: &str = "00000000-0000-4000-8000-000000000002";
    const REQUEST_1: &str = "00000000-0000-4000-8000-000000000011";
    const RUN: &str = "00000000-0000-4000-8000-000000000021";
    const TASK: &str = "00000000-0000-4000-8000-000000000022";
    const TURN: &str = "00000000-0000-4000-8000-000000000023";
    const SCOPE: &str = "00000000-0000-4000-8000-000000000024";
    const OPERATION: &str = "00000000-0000-4000-8000-000000000025";
    const EFFECT: &str = "00000000-0000-4000-8000-000000000026";

    fn protocol() -> ProtocolIdentity {
        ProtocolIdentity {
            name: "focus-agent.platform".into(),
            version: ProtocolVersion { major: 1, minor: 0 },
            active_features: ActiveFeatures::default(),
            schema_digest: SchemaDigest::from_bytes([0x11; 32]),
        }
    }

    fn profile() -> NegotiatedContractProfile {
        let protocol = protocol();
        NegotiatedContractProfile::new(
            protocol.name,
            protocol.version,
            protocol.active_features,
            protocol.schema_digest,
        )
        .unwrap()
    }

    fn work() -> WorkIdentity {
        WorkIdentity {
            run_id: RunId::from_str(RUN).unwrap(),
            task_id: Some(TaskId::from_str(TASK).unwrap()),
            turn_id: Some(TurnId::from_str(TURN).unwrap()),
            scope_id: Some(ScopeId::from_str(SCOPE).unwrap()),
            operation_id: OperationId::from_str(OPERATION).unwrap(),
            generation: 7,
            attempt: Attempt::new(1).unwrap(),
            call_id: Some("call_model_1".into()),
            effect_id: None,
            argument_digest: ArgumentDigest::from_bytes([0x22; 32]),
            deadline_remaining_ms: DeadlineRemainingMs::new(30_000).unwrap(),
            authority_ref: None,
        }
    }

    fn request<P>(route: Route, payload: P) -> PlatformEnvelope<P> {
        let message_id = MessageId::from_str(MESSAGE_1).unwrap();
        PlatformEnvelope {
            protocol: protocol(),
            message_id,
            request_id: Some(RequestId::from_str(REQUEST_1).unwrap()),
            kind: EnvelopeKind::Request,
            route,
            work: Some(work()),
            causality: Causality::root(message_id),
            payload,
        }
    }

    fn response<RequestPayload, ResponsePayload>(
        request: &PlatformEnvelope<RequestPayload>,
        value: ResponsePayload,
    ) -> PlatformEnvelope<PlatformResponse<ResponsePayload>> {
        let mut response_work = request.work.clone().unwrap();
        response_work.deadline_remaining_ms = DeadlineRemainingMs::new(29_000).unwrap();
        PlatformEnvelope {
            protocol: request.protocol.clone(),
            message_id: MessageId::from_str(MESSAGE_2).unwrap(),
            request_id: request.request_id,
            kind: EnvelopeKind::Response,
            route: request.route.clone(),
            work: Some(response_work),
            causality: Causality::caused_by(request.causality.correlation_id, request.message_id),
            payload: PlatformResponse::Success { value },
        }
    }

    fn snapshot(state: OperationState) -> OperationSnapshot {
        OperationSnapshot {
            identity: ToolOperationIdentity {
                run_id: RunId::from_str(RUN).unwrap(),
                task_id: Some(TaskId::from_str(TASK).unwrap()),
                turn_id: TurnId::from_str(TURN).unwrap(),
                scope_id: Some(ScopeId::from_str(SCOPE).unwrap()),
                operation_id: OperationId::from_str(OPERATION).unwrap(),
                generation: 7,
                call_id: "call_model_1".into(),
                tool_name: "fs.write".into(),
                argument_digest: ArgumentDigest::from_bytes([0x22; 32]),
            },
            state,
        }
    }

    fn found(state: OperationState) -> OperationQueryResult {
        OperationQueryResult::Found {
            snapshot: Box::new(snapshot(state)),
        }
    }

    #[test]
    fn operation_query_request_has_exact_golden_shape() {
        let request = request(Route::operation_query(), OperationQueryRequest {});
        validate_operation_query_request(&profile(), &request).unwrap();

        let digest_11 = "11".repeat(32);
        let digest_22 = "22".repeat(32);
        let expected = format!(
            "{{\"protocol\":{{\"name\":\"focus-agent.platform\",\"version\":{{\"major\":1,\"minor\":0}},\"active_features\":[],\"schema_digest\":\"{digest_11}\"}},\"message_id\":\"{MESSAGE_1}\",\"request_id\":\"{REQUEST_1}\",\"kind\":\"request\",\"route\":{{\"namespace\":\"operation\",\"operation\":\"query\"}},\"work\":{{\"run_id\":\"{RUN}\",\"task_id\":\"{TASK}\",\"turn_id\":\"{TURN}\",\"scope_id\":\"{SCOPE}\",\"operation_id\":\"{OPERATION}\",\"generation\":7,\"attempt\":1,\"call_id\":\"call_model_1\",\"argument_digest\":\"{digest_22}\",\"deadline_remaining_ms\":30000}},\"causality\":{{\"correlation_id\":\"{MESSAGE_1}\"}},\"payload\":{{}}}}"
        );
        assert_eq!(serde_json::to_string(&request).unwrap(), expected);
        assert_eq!(
            serde_json::from_str::<PlatformEnvelope<OperationQueryRequest>>(&expected).unwrap(),
            request
        );
    }

    #[test]
    fn operation_dtos_reject_unknown_fields() {
        assert!(serde_json::from_value::<OperationQueryRequest>(json!({"extra": true})).is_err());
        assert!(serde_json::from_value::<OperationCancelRequest>(json!({"extra": true})).is_err());
        assert!(
            serde_json::from_value::<OperationQueryResponse>(
                json!({"result": {"status": "not_found"}, "extra": true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<OperationCancelAck>(
                json!({"result": {"status": "not_found"}, "extra": true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<OperationAccepted>(
                json!({"disposition": "accepted", "result": {"status": "not_found"}, "extra": true})
            )
            .is_err()
        );

        let mut nested_snapshot = serde_json::to_value(OperationQueryResponse {
            result: found(OperationState::Accepted),
        })
        .unwrap();
        nested_snapshot["result"]["snapshot"]["extra"] = json!(true);
        assert!(serde_json::from_value::<OperationQueryResponse>(nested_snapshot).is_err());

        let mut nested_identity = serde_json::to_value(OperationCancelAck {
            result: found(OperationState::Accepted),
        })
        .unwrap();
        nested_identity["result"]["snapshot"]["identity"]["extra"] = json!(true);
        assert!(serde_json::from_value::<OperationCancelAck>(nested_identity).is_err());

        let mut nested_state = serde_json::to_value(OperationQueryResponse {
            result: found(OperationState::Executing { effect_id: None }),
        })
        .unwrap();
        nested_state["result"]["snapshot"]["state"]["extra"] = json!(true);
        assert!(serde_json::from_value::<OperationQueryResponse>(nested_state).is_err());
    }

    #[test]
    fn request_validation_pins_route_kind_and_nonzero_generation() {
        let query = request(Route::operation_query(), OperationQueryRequest {});
        validate_operation_query_request(&profile(), &query).unwrap();

        let wrong_route = request(Route::operation_cancel(), OperationQueryRequest {});
        assert!(validate_operation_query_request(&profile(), &wrong_route).is_err());

        let mut wrong_kind = query.clone();
        wrong_kind.kind = EnvelopeKind::Response;
        assert!(validate_operation_query_request(&profile(), &wrong_kind).is_err());

        let mut zero_generation = query;
        zero_generation.work.as_mut().unwrap().generation = 0;
        assert!(validate_operation_query_request(&profile(), &zero_generation).is_err());

        let mut effect_qualified = request(Route::operation_query(), OperationQueryRequest {});
        effect_qualified.work.as_mut().unwrap().effect_id =
            Some(EffectId::from_str(EFFECT).unwrap());
        assert!(validate_operation_query_request(&profile(), &effect_qualified).is_err());

        let mut missing_turn = request(Route::operation_query(), OperationQueryRequest {});
        missing_turn.work.as_mut().unwrap().turn_id = None;
        assert!(validate_operation_query_request(&profile(), &missing_turn).is_err());

        let mut exhausted = request(Route::operation_query(), OperationQueryRequest {});
        exhausted.work.as_mut().unwrap().deadline_remaining_ms =
            DeadlineRemainingMs::new(0).unwrap();
        assert!(validate_operation_query_request(&profile(), &exhausted).is_err());

        let cancel = request(Route::operation_cancel(), OperationCancelRequest {});
        validate_operation_cancel_request(&profile(), &cancel).unwrap();
    }

    #[test]
    fn response_rejects_invalid_nested_result_and_identity_drift() {
        let request = request(Route::operation_query(), OperationQueryRequest {});
        let mut invalid = snapshot(OperationState::Accepted);
        invalid.identity.call_id.clear();
        let invalid = response(
            &request,
            OperationQueryResponse {
                result: OperationQueryResult::Found {
                    snapshot: Box::new(invalid),
                },
            },
        );
        assert!(validate_operation_query_response(&profile(), &request, &invalid).is_err());

        let mut drifted = snapshot(OperationState::Accepted);
        drifted.identity.operation_id = OperationId::new();
        let drifted = response(
            &request,
            OperationQueryResponse {
                result: OperationQueryResult::Found {
                    snapshot: Box::new(drifted),
                },
            },
        );
        let error = validate_operation_query_response(&profile(), &request, &drifted).unwrap_err();
        assert_eq!(error.field(), "operation.result.identity");
    }

    #[test]
    fn cancel_ack_preserves_commit_started_and_applied_truth() {
        let request = request(Route::operation_cancel(), OperationCancelRequest {});
        let effect_id = EffectId::from_str(EFFECT).unwrap();
        let commit_started_result = found(OperationState::CommitStarted { effect_id });
        let commit_started = response(
            &request,
            OperationCancelAck {
                result: commit_started_result.clone(),
            },
        );
        validate_operation_cancel_response(&profile(), &request, &commit_started).unwrap();
        assert_eq!(
            serde_json::from_value::<OperationCancelAck>(
                serde_json::to_value(&commit_started.payload).unwrap()["value"].clone()
            )
            .unwrap()
            .result,
            commit_started_result
        );

        let applied_result = found(OperationState::Terminal {
            effect_id: Some(effect_id),
            terminal: OperationTerminal::Applied {
                durability: EffectDurability::Durable,
                evidence: Some("workspace-mutation-1".into()),
            },
        });
        let applied = response(
            &request,
            OperationCancelAck {
                result: applied_result.clone(),
            },
        );
        validate_operation_cancel_response(&profile(), &request, &applied).unwrap();
        let encoded = serde_json::to_string(&applied.payload).unwrap();
        assert!(!encoded.contains("commit_started"));
        assert!(encoded.contains("\"applied\""));
        assert!(!encoded.contains("cancelled"));
        let PlatformResponse::Success { value } = applied.payload else {
            unreachable!()
        };
        assert_eq!(value.result, applied_result);
    }

    #[test]
    fn accepted_body_requires_found_truth_for_both_dispositions() {
        for disposition in [
            AdmissionDisposition::Accepted,
            AdmissionDisposition::AlreadyKnown,
        ] {
            let accepted = OperationAccepted {
                disposition,
                result: found(OperationState::Accepted),
            };
            accepted.validate_against_work(&work()).unwrap();
        }

        for result in [
            OperationQueryResult::NotFound,
            OperationQueryResult::ExpiredOrPossiblySeen,
        ] {
            assert!(
                OperationAccepted {
                    disposition: AdmissionDisposition::AlreadyKnown,
                    result,
                }
                .validate()
                .is_err()
            );
        }
    }
}

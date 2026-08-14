//! Transport-independent Platform operation-control routing.
//!
//! This is the policy/dispatch seam between already-authenticated connection
//! state and the sole [`RuntimeActor`](crate::actor::RuntimeActor). It owns no
//! socket, framing, task loop or Core handle: transports validate bytes and
//! negotiate a profile, a trusted authorizer resolves the opaque authority
//! reference, and every query/cancellation is serialized through
//! [`RuntimeHandle`].
//!
//! 连接会话在握手时由可信组合根安装；对等端带的 `authority_ref` 不是授权。
//! 有界 JSON-lines 传输在 `agent_process::FramedProtocolSession`；适配器
//! 只吃已经读完的一帧正文，不拥有 pipe/socket。

mod session;

use std::{sync::Arc, time::Instant};

use agent_contracts::{
    AgentError, AgentResult, OperationQueryResult, OperationState, OperationTerminal, RunId,
    RuntimeEvent,
};
use agent_platform_protocol::{
    AdmissionDisposition, EffectStateDisposition, EnvelopeKind, MessageId,
    NegotiatedContractProfile, OperationAccepted, OperationCancelAck, OperationCancelRequest,
    OperationQueryRequest, OperationQueryResponse, PlatformEnvelope, PlatformError,
    PlatformErrorClass, PlatformResponse, RetryDisposition, validate_operation_cancel_request,
    validate_operation_cancel_response, validate_operation_query_request,
    validate_operation_query_response,
};

use crate::RuntimeHandle;

pub use session::{
    AuthenticatedOperationControlAdapter, BoundSessionAuthorizer,
    MAX_OPERATION_CONTROL_ENVELOPE_BYTES, MAX_OPERATION_CONTROL_SESSIONS, OperationControlGrant,
    OperationControlSessionRegistry,
};

/// Permission being requested from trusted, connection-scoped policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationControlAction {
    ObserveAccepted,
    Query,
    Cancel,
}

/// Bounded facts supplied to the trusted authorizer. `authority_ref` is only
/// an opaque lookup key; possession of the string does not grant authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationControlAuthorizationRequest {
    pub action: OperationControlAction,
    pub run_id: RunId,
    pub operation_id: agent_contracts::OperationId,
    pub authority_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationControlAuthorization {
    Authorized,
    Denied,
}

/// Resolves an already-authenticated session/grant at the Platform boundary.
/// Implementations belong to trusted composition; wire peers cannot mint one.
/// Connection-bound implementations must ignore wire `authority_ref`.
pub trait OperationControlAuthorizer: Send + Sync {
    fn authorize(
        &self,
        request: &OperationControlAuthorizationRequest,
    ) -> OperationControlAuthorization;
}

/// Authorized, transport-independent query/cancel router. The router is a
/// facade over the actor command channel, not an authority registry or a
/// scheduler. A Named Pipe, UDS, inherited-pipe or in-process adapter can all
/// reuse it without changing operation semantics.
pub struct OperationControlRouter {
    profile: NegotiatedContractProfile,
    runtime: RuntimeHandle,
    authorizer: Arc<dyn OperationControlAuthorizer>,
}

/// Authorized live projection of WAL-first operation admissions. It is
/// intentionally bounded by Runtime's broadcast channel; lag is surfaced as
/// an error rather than silently skipping identities. Durable replay remains
/// a transport/session concern, while Core query remains the truth source.
pub struct OperationAcceptedSubscription {
    receiver: tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    run_id: RunId,
    authorizer: Arc<dyn OperationControlAuthorizer>,
}

impl OperationAcceptedSubscription {
    pub async fn recv(&mut self) -> AgentResult<OperationAccepted> {
        loop {
            let envelope = self.receiver.recv().await.map_err(|error| match error {
                tokio::sync::broadcast::error::RecvError::Closed => AgentError::Transport {
                    retryable: false,
                    message: "runtime operation-acceptance stream closed".into(),
                },
                tokio::sync::broadcast::error::RecvError::Lagged(skipped) => {
                    AgentError::Transport {
                        retryable: false,
                        message: format!(
                            "runtime operation-acceptance stream lagged by {skipped} bounded event(s); missing identities must not be guessed"
                        ),
                    }
                }
            })?;
            if envelope.run_id != self.run_id {
                continue;
            }
            let RuntimeEvent::OperationAccepted { snapshot } = envelope.event else {
                continue;
            };
            let authorization = OperationControlAuthorizationRequest {
                action: OperationControlAction::ObserveAccepted,
                run_id: snapshot.identity.run_id,
                operation_id: snapshot.identity.operation_id,
                authority_ref: None,
            };
            if self.authorizer.authorize(&authorization)
                != OperationControlAuthorization::Authorized
            {
                continue;
            }
            let accepted = OperationAccepted {
                disposition: AdmissionDisposition::Accepted,
                result: OperationQueryResult::Found { snapshot },
            };
            accepted
                .validate()
                .map_err(|error| AgentError::Internal(error.to_string()))?;
            return Ok(accepted);
        }
    }
}

impl OperationControlRouter {
    pub fn new(
        profile: NegotiatedContractProfile,
        runtime: RuntimeHandle,
        authorizer: Arc<dyn OperationControlAuthorizer>,
    ) -> AgentResult<Self> {
        profile
            .validate()
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            profile,
            runtime,
            authorizer,
        })
    }

    /// Subscribe to newly admitted operations through the same trusted
    /// authorizer used for query/cancel. The event is published only after
    /// Core returns a WAL-backed admission snapshot.
    pub fn subscribe_accepted(&self) -> OperationAcceptedSubscription {
        OperationAcceptedSubscription {
            receiver: self.runtime.subscribe(),
            run_id: self.runtime.run_id(),
            authorizer: self.authorizer.clone(),
        }
    }

    /// Route a read-only query. `Found`, conservative expiry and genuine
    /// `NotFound` remain distinct success values; authorization failure is
    /// deliberately indistinguishable from a foreign run.
    pub async fn query(
        &self,
        request: PlatformEnvelope<OperationQueryRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<OperationQueryResponse>>> {
        validate_operation_query_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(OperationControlAction::Query, &request) {
            return self.query_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }

        let work = request
            .work
            .as_ref()
            .expect("validated operation request carries work");
        let operation_id = work.operation_id;
        let remaining = work
            .deadline_remaining_ms
            .after_elapsed(elapsed_ms(started));
        if remaining.is_exhausted() {
            return self.query_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: deadline_error(OperationControlAction::Query),
                },
            );
        }
        let result = match tokio::time::timeout(
            std::time::Duration::from_millis(u64::from(remaining.as_millis())),
            self.runtime.query_operation(operation_id),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return self.query_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: runtime_error(error, OperationControlAction::Query),
                    },
                );
            }
            Err(_) => {
                return self.query_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: deadline_error(OperationControlAction::Query),
                    },
                );
            }
        };
        let value = OperationQueryResponse { result };
        let work = request
            .work
            .as_ref()
            .expect("validated operation request carries work");
        if value.validate_against_work(work).is_err() {
            return self.query_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: identity_mismatch_error(),
                },
            );
        }
        self.query_response(&request, started, PlatformResponse::Success { value })
    }

    /// Cancel only the exact current tool operation. The router first reads
    /// Core truth through the actor, canonicalizes the complete identity from
    /// the returned snapshot, and then asks the same actor to cancel it.
    /// A pre-existing Core cancellation terminal is not enough to prove that
    /// Runtime completed scope cleanup and the durable `TurnCancelled`
    /// barrier, so this router never upgrades that truth into a success ACK.
    pub async fn cancel(
        &self,
        request: PlatformEnvelope<OperationCancelRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<OperationCancelAck>>> {
        validate_operation_cancel_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(OperationControlAction::Cancel, &request) {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }

        let work = request
            .work
            .as_ref()
            .expect("validated operation request carries work");
        let remaining = work
            .deadline_remaining_ms
            .after_elapsed(elapsed_ms(started));
        if remaining.is_exhausted() {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: deadline_error(OperationControlAction::Cancel),
                },
            );
        }
        let query = match tokio::time::timeout(
            std::time::Duration::from_millis(u64::from(remaining.as_millis())),
            self.runtime.query_operation(work.operation_id),
        )
        .await
        {
            Ok(Ok(query)) => query,
            Ok(Err(error)) => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: runtime_error(error, OperationControlAction::Cancel),
                    },
                );
            }
            Err(_) => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: deadline_error(OperationControlAction::Cancel),
                    },
                );
            }
        };

        let snapshot = match query {
            OperationQueryResult::Found { snapshot } => snapshot,
            OperationQueryResult::NotFound => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: not_found_error(),
                    },
                );
            }
            OperationQueryResult::ExpiredOrPossiblySeen => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: history_expired_error(),
                    },
                );
            }
        };
        let canonical = OperationQueryResponse {
            result: OperationQueryResult::Found {
                snapshot: snapshot.clone(),
            },
        };
        if canonical.validate_against_work(work).is_err() {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: identity_mismatch_error(),
                },
            );
        }

        if is_cancelled(&snapshot.state) {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: cancellation_ack_unavailable_error(),
                },
            );
        }
        if !is_cancellable(&snapshot.state) {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: not_cancellable_error(&snapshot.state),
                },
            );
        }

        let remaining = work
            .deadline_remaining_ms
            .after_elapsed(elapsed_ms(started));
        if remaining.is_exhausted() {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: deadline_error(OperationControlAction::Cancel),
                },
            );
        }
        let result = match tokio::time::timeout(
            std::time::Duration::from_millis(u64::from(remaining.as_millis())),
            self.runtime.cancel_operation(snapshot.identity.clone()),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: runtime_error(error, OperationControlAction::Cancel),
                    },
                );
            }
            Err(_) => {
                return self.cancel_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: deadline_error(OperationControlAction::Cancel),
                    },
                );
            }
        };
        let value = OperationCancelAck { result };
        if value.validate_against_work(work).is_err() {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: identity_mismatch_error(),
                },
            );
        }
        match &value.result {
            OperationQueryResult::Found { snapshot } if is_cancelled(&snapshot.state) => {
                self.cancel_response(&request, started, PlatformResponse::Success { value })
            }
            OperationQueryResult::Found { snapshot } => self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: not_cancellable_error(&snapshot.state),
                },
            ),
            OperationQueryResult::NotFound => self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: not_found_error(),
                },
            ),
            OperationQueryResult::ExpiredOrPossiblySeen => self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: history_expired_error(),
                },
            ),
        }
    }

    fn is_authorized<P>(
        &self,
        action: OperationControlAction,
        request: &PlatformEnvelope<P>,
    ) -> bool {
        let work = request
            .work
            .as_ref()
            .expect("validated operation request carries work");
        work.run_id == self.runtime.run_id()
            && self
                .authorizer
                .authorize(&OperationControlAuthorizationRequest {
                    action,
                    run_id: work.run_id,
                    operation_id: work.operation_id,
                    // 只是查找键；连接绑定的授权器必须忽略它。
                    authority_ref: work.authority_ref.clone(),
                })
                == OperationControlAuthorization::Authorized
    }

    fn query_response(
        &self,
        request: &PlatformEnvelope<OperationQueryRequest>,
        started: Instant,
        payload: PlatformResponse<OperationQueryResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<OperationQueryResponse>>> {
        let response = response_envelope(request, started, payload);
        validate_operation_query_response(&self.profile, request, &response)
            .map_err(|error| AgentError::Internal(error.to_string()))?;
        Ok(response)
    }

    fn cancel_response(
        &self,
        request: &PlatformEnvelope<OperationCancelRequest>,
        started: Instant,
        payload: PlatformResponse<OperationCancelAck>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<OperationCancelAck>>> {
        let response = response_envelope(request, started, payload);
        validate_operation_cancel_response(&self.profile, request, &response)
            .map_err(|error| AgentError::Internal(error.to_string()))?;
        Ok(response)
    }
}

fn response_envelope<RequestPayload, ResponsePayload>(
    request: &PlatformEnvelope<RequestPayload>,
    started: Instant,
    payload: PlatformResponse<ResponsePayload>,
) -> PlatformEnvelope<PlatformResponse<ResponsePayload>> {
    let mut work = request
        .work
        .clone()
        .expect("validated operation request carries work");
    work.deadline_remaining_ms = work
        .deadline_remaining_ms
        .after_elapsed(elapsed_ms(started));
    PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: MessageId::new(),
        request_id: request.request_id,
        kind: EnvelopeKind::Response,
        route: request.route.clone(),
        work: Some(work),
        causality: agent_platform_protocol::Causality::caused_by(
            request.causality.correlation_id,
            request.message_id,
        ),
        payload,
    }
}

fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)
}

fn is_cancellable(state: &OperationState) -> bool {
    matches!(
        state,
        OperationState::Accepted
            | OperationState::Executing { .. }
            | OperationState::Prepared { .. }
    )
}

fn is_cancelled(state: &OperationState) -> bool {
    matches!(
        state,
        OperationState::Terminal {
            terminal: OperationTerminal::CancelledBeforeCommit,
            ..
        }
    )
}

fn platform_error(
    class: PlatformErrorClass,
    code: &str,
    message: &str,
    retry: RetryDisposition,
    effect_state: EffectStateDisposition,
) -> PlatformError {
    PlatformError {
        class,
        code: code.into(),
        message: message.into(),
        retry,
        effect_state,
        retry_after_ms: None,
        diagnostic_ref: None,
    }
}

fn forbidden_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Domain,
        "operation.forbidden",
        "operation is not visible or controllable under the installed Platform authority",
        RetryDisposition::Never,
        EffectStateDisposition::NotApplicable,
    )
}

fn identity_mismatch_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Protocol,
        "protocol.operation_identity_mismatch",
        "operation work identity does not match Core authority truth",
        RetryDisposition::Never,
        EffectStateDisposition::NotApplicable,
    )
}

fn not_found_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Domain,
        "operation.not_found",
        "operation is not present in Core authority history",
        RetryDisposition::Never,
        EffectStateDisposition::NotApplicable,
    )
}

fn history_expired_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Domain,
        "operation.history_expired",
        "operation may be known but its exact retained state has expired",
        RetryDisposition::QueryBeforeRetry,
        EffectStateDisposition::OutcomeUnknown,
    )
}

fn cancellation_ack_unavailable_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Domain,
        "operation.cancellation_ack_unavailable",
        "Core records cancellation, but this request has no durable Runtime cancellation acknowledgement",
        RetryDisposition::Never,
        EffectStateDisposition::NotApplicable,
    )
}

fn deadline_error(action: OperationControlAction) -> PlatformError {
    match action {
        OperationControlAction::ObserveAccepted => platform_error(
            PlatformErrorClass::Domain,
            "operation.control_unavailable",
            "operation acceptance publication is unavailable",
            RetryDisposition::Never,
            EffectStateDisposition::NotApplicable,
        ),
        OperationControlAction::Query => platform_error(
            PlatformErrorClass::Domain,
            "operation.deadline_exceeded",
            "operation query exceeded its remaining monotonic deadline",
            RetryDisposition::SameOperation,
            EffectStateDisposition::NotApplicable,
        ),
        OperationControlAction::Cancel => platform_error(
            PlatformErrorClass::Domain,
            "operation.deadline_exceeded",
            "operation cancellation deadline expired; query before any retry",
            RetryDisposition::QueryBeforeRetry,
            EffectStateDisposition::OutcomeUnknown,
        ),
    }
}

fn runtime_error(error: AgentError, action: OperationControlAction) -> PlatformError {
    match error {
        AgentError::RecoveryRequired(_) => platform_error(
            PlatformErrorClass::Domain,
            "operation.recovery_required",
            "Core or Runtime authority is recovery-fenced; query is required before retry",
            RetryDisposition::QueryBeforeRetry,
            EffectStateDisposition::OutcomeUnknown,
        ),
        AgentError::InvalidRequest(_) if action == OperationControlAction::Cancel => {
            platform_error(
                PlatformErrorClass::Domain,
                "operation.not_cancellable",
                "operation is not the exact current cancellable tool operation",
                RetryDisposition::Never,
                EffectStateDisposition::OutcomeUnknown,
            )
        }
        _ => platform_error(
            PlatformErrorClass::Domain,
            "operation.control_unavailable",
            "Platform operation-control service could not determine authoritative state",
            RetryDisposition::QueryBeforeRetry,
            EffectStateDisposition::OutcomeUnknown,
        ),
    }
}

fn not_cancellable_error(state: &OperationState) -> PlatformError {
    let effect_state = match state {
        OperationState::CommitStarted { .. } => EffectStateDisposition::OutcomeUnknown,
        OperationState::Terminal { terminal, .. } => match terminal {
            OperationTerminal::Applied { .. } => EffectStateDisposition::Applied,
            OperationTerminal::NotApplied { .. } | OperationTerminal::Refused { .. } => {
                EffectStateDisposition::NotApplied
            }
            OperationTerminal::OutcomeUnknown { .. } => EffectStateDisposition::OutcomeUnknown,
            OperationTerminal::CompletedValue | OperationTerminal::CancelledBeforeCommit => {
                EffectStateDisposition::NotApplicable
            }
        },
        OperationState::Accepted
        | OperationState::Executing { .. }
        | OperationState::Prepared { .. } => EffectStateDisposition::OutcomeUnknown,
    };
    platform_error(
        PlatformErrorClass::Domain,
        "operation.not_cancellable",
        "operation is no longer in a state this Platform route may cancel",
        RetryDisposition::Never,
        effect_state,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use agent_contracts::{
        ArgumentDigest, OperationId, OperationSnapshot, TaskId, ToolOperationIdentity, TurnId,
    };
    use agent_platform_protocol::{
        ActiveFeatures, Attempt, Causality, DeadlineRemainingMs, ProtocolIdentity, ProtocolVersion,
        RequestId, Route, SchemaDigest, WorkIdentity,
    };
    use tokio::sync::{broadcast, mpsc};

    use super::*;
    use crate::command::{RuntimeCommand, RuntimeHandle};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct FixedAuthorizer(OperationControlAuthorization);

    impl OperationControlAuthorizer for FixedAuthorizer {
        fn authorize(
            &self,
            _request: &OperationControlAuthorizationRequest,
        ) -> OperationControlAuthorization {
            self.0
        }
    }

    struct SlowAuthorizer(Duration);

    impl OperationControlAuthorizer for SlowAuthorizer {
        fn authorize(
            &self,
            _request: &OperationControlAuthorizationRequest,
        ) -> OperationControlAuthorization {
            std::thread::sleep(self.0);
            OperationControlAuthorization::Authorized
        }
    }

    fn profile() -> NegotiatedContractProfile {
        NegotiatedContractProfile::new(
            "focus-agent.platform",
            ProtocolVersion { major: 1, minor: 0 },
            ActiveFeatures::default(),
            SchemaDigest::from_bytes([0x11; 32]),
        )
        .unwrap()
    }

    fn identity(run_id: RunId) -> ToolOperationIdentity {
        ToolOperationIdentity {
            run_id,
            task_id: Some(TaskId::new()),
            turn_id: TurnId::new(),
            scope_id: None,
            operation_id: OperationId::new(),
            generation: 7,
            call_id: "call-1".into(),
            tool_name: "fs.read".into(),
            argument_digest: ArgumentDigest::sha256_bytes(b"args"),
        }
    }

    fn work(identity: &ToolOperationIdentity) -> WorkIdentity {
        WorkIdentity {
            run_id: identity.run_id,
            task_id: identity.task_id,
            turn_id: Some(identity.turn_id),
            scope_id: identity.scope_id,
            operation_id: identity.operation_id,
            generation: identity.generation,
            attempt: Attempt::new(1).unwrap(),
            call_id: Some(identity.call_id.clone()),
            effect_id: None,
            argument_digest: identity.argument_digest,
            deadline_remaining_ms: DeadlineRemainingMs::new(5_000).unwrap(),
            authority_ref: Some("session-grant-1".into()),
        }
    }

    fn request<P>(route: Route, work: WorkIdentity, payload: P) -> PlatformEnvelope<P> {
        let message_id = MessageId::new();
        PlatformEnvelope {
            protocol: ProtocolIdentity {
                name: "focus-agent.platform".into(),
                version: ProtocolVersion { major: 1, minor: 0 },
                active_features: ActiveFeatures::default(),
                schema_digest: SchemaDigest::from_bytes([0x11; 32]),
            },
            message_id,
            request_id: Some(RequestId::new()),
            kind: EnvelopeKind::Request,
            route,
            work: Some(work),
            causality: Causality::root(message_id),
            payload,
        }
    }

    fn handle(
        run_id: RunId,
    ) -> (
        RuntimeHandle,
        mpsc::Receiver<RuntimeCommand>,
        broadcast::Sender<agent_contracts::RuntimeEventEnvelope>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let (events, _) = broadcast::channel(8);
        (RuntimeHandle::new(tx, events.clone(), run_id), rx, events)
    }

    #[tokio::test]
    async fn denied_query_never_enters_the_actor() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (handle, mut commands, _events) = handle(run_id);
        let router = OperationControlRouter::new(
            profile(),
            handle,
            Arc::new(FixedAuthorizer(OperationControlAuthorization::Denied)),
        )
        .unwrap();
        let response = router
            .query(request(
                Route::operation_query(),
                work(&operation),
                OperationQueryRequest {},
            ))
            .await
            .unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error } if error.code == "operation.forbidden"
        ));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn authorization_consumes_deadline_before_actor_dispatch() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (handle, mut commands, _events) = handle(run_id);
        let router = OperationControlRouter::new(
            profile(),
            handle,
            Arc::new(SlowAuthorizer(Duration::from_millis(5))),
        )
        .unwrap();
        let mut expiring = work(&operation);
        expiring.deadline_remaining_ms = DeadlineRemainingMs::new(1).unwrap();

        let response = router
            .query(request(
                Route::operation_query(),
                expiring,
                OperationQueryRequest {},
            ))
            .await
            .unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error }
                if error.code == "operation.deadline_exceeded"
        ));
        assert!(
            commands.try_recv().is_err(),
            "an expired post-authorization deadline must not enter the actor"
        );
    }

    #[tokio::test]
    async fn accepted_subscription_projects_only_wal_first_runtime_event() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (handle, _commands, events) = handle(run_id);
        let router = OperationControlRouter::new(
            profile(),
            handle,
            Arc::new(FixedAuthorizer(OperationControlAuthorization::Authorized)),
        )
        .unwrap();
        let mut accepted = router.subscribe_accepted();
        events
            .send(agent_contracts::RuntimeEventEnvelope {
                run_id,
                seq: 1,
                timestamp_ms: 1,
                event: RuntimeEvent::OperationAccepted {
                    snapshot: Box::new(OperationSnapshot {
                        identity: operation.clone(),
                        state: OperationState::Accepted,
                    }),
                },
            })
            .unwrap();
        let publication = accepted.recv().await.unwrap();
        assert_eq!(publication.disposition, AdmissionDisposition::Accepted);
        assert!(matches!(
            publication.result,
            OperationQueryResult::Found { snapshot }
                if snapshot.identity == operation && snapshot.state == OperationState::Accepted
        ));
    }

    #[tokio::test]
    async fn query_canonicalizes_found_identity_and_rejects_drift() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (handle, mut commands, _events) = handle(run_id);
        let router = OperationControlRouter::new(
            profile(),
            handle,
            Arc::new(FixedAuthorizer(OperationControlAuthorization::Authorized)),
        )
        .unwrap();
        let snapshot = OperationSnapshot {
            identity: operation.clone(),
            state: OperationState::Accepted,
        };
        tokio::spawn(async move {
            let Some(RuntimeCommand::QueryOperation { reply, .. }) = commands.recv().await else {
                panic!("router did not issue the actor query command");
            };
            let _ = reply.send(Ok(OperationQueryResult::Found {
                snapshot: Box::new(snapshot),
            }));
        });
        let mut drifted = work(&operation);
        drifted.call_id = Some("other-call".into());
        let response = router
            .query(request(
                Route::operation_query(),
                drifted,
                OperationQueryRequest {},
            ))
            .await
            .unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error }
                if error.code == "protocol.operation_identity_mismatch"
        ));
    }

    #[tokio::test]
    async fn core_cancelled_truth_alone_is_not_a_runtime_cancel_ack() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (handle, mut commands, _events) = handle(run_id);
        let router = OperationControlRouter::new(
            profile(),
            handle,
            Arc::new(FixedAuthorizer(OperationControlAuthorization::Authorized)),
        )
        .unwrap();
        let snapshot = OperationSnapshot {
            identity: operation.clone(),
            state: OperationState::Terminal {
                effect_id: None,
                terminal: OperationTerminal::CancelledBeforeCommit,
            },
        };
        tokio::spawn(async move {
            let Some(RuntimeCommand::QueryOperation { reply, .. }) = commands.recv().await else {
                panic!("router did not query before cancellation");
            };
            let _ = reply.send(Ok(OperationQueryResult::Found {
                snapshot: Box::new(snapshot),
            }));
        });
        let response = router
            .cancel(request(
                Route::operation_cancel(),
                work(&operation),
                OperationCancelRequest {},
            ))
            .await
            .unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error }
                if error.code == "operation.cancellation_ack_unavailable"
        ));
    }

    #[test]
    fn session_registry_bounds_live_grants() {
        let registry = OperationControlSessionRegistry::new(RunId::new());
        for _ in 0..MAX_OPERATION_CONTROL_SESSIONS {
            registry
                .install(OperationControlGrant::query_only())
                .unwrap();
        }
        assert!(
            registry
                .install(OperationControlGrant::query_only())
                .is_err()
        );
    }

    #[tokio::test]
    async fn connection_grant_ignores_forged_wire_authority_ref() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry
            .install(OperationControlGrant::query_only())
            .unwrap();
        let adapter =
            AuthenticatedOperationControlAdapter::bind(profile(), runtime, registry, &session_id)
                .unwrap();
        let mut forged = work(&operation);
        forged.authority_ref = Some("admin-grant".into());
        let bytes = serde_json::to_vec(&request(
            Route::operation_cancel(),
            forged,
            OperationCancelRequest {},
        ))
        .unwrap();
        let response_bytes = adapter.handle_frame(&bytes).await.unwrap();
        let response: PlatformEnvelope<PlatformResponse<OperationCancelAck>> =
            serde_json::from_slice(&response_bytes).unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error } if error.code == "operation.forbidden"
        ));
        assert!(
            commands.try_recv().is_err(),
            "a forged authority_ref must not enter the actor"
        );
        assert_eq!(
            response.work.as_ref().unwrap().authority_ref.as_deref(),
            Some(session_id.as_str()),
            "the adapter must stamp the connection session, not the peer-supplied ref"
        );
    }

    #[tokio::test]
    async fn bound_session_query_reaches_the_actor() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry.install(OperationControlGrant::operator()).unwrap();
        let adapter =
            AuthenticatedOperationControlAdapter::bind(profile(), runtime, registry, &session_id)
                .unwrap();
        tokio::spawn(async move {
            let Some(RuntimeCommand::QueryOperation { reply, .. }) = commands.recv().await else {
                panic!("bound session query must reach the actor");
            };
            let _ = reply.send(Ok(OperationQueryResult::NotFound));
        });
        let bytes = serde_json::to_vec(&request(
            Route::operation_query(),
            work(&operation),
            OperationQueryRequest {},
        ))
        .unwrap();
        let response_bytes = adapter.handle_frame(&bytes).await.unwrap();
        let response: PlatformEnvelope<PlatformResponse<OperationQueryResponse>> =
            serde_json::from_slice(&response_bytes).unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Success {
                value: OperationQueryResponse {
                    result: OperationQueryResult::NotFound,
                },
            }
        ));
    }

    #[tokio::test]
    async fn revoked_session_is_denied_without_entering_the_actor() {
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry.install(OperationControlGrant::operator()).unwrap();
        let adapter = AuthenticatedOperationControlAdapter::bind(
            profile(),
            runtime,
            registry.clone(),
            &session_id,
        )
        .unwrap();
        registry.revoke(&session_id).unwrap();
        let bytes = serde_json::to_vec(&request(
            Route::operation_query(),
            work(&operation),
            OperationQueryRequest {},
        ))
        .unwrap();
        let response_bytes = adapter.handle_frame(&bytes).await.unwrap();
        let response: PlatformEnvelope<PlatformResponse<OperationQueryResponse>> =
            serde_json::from_slice(&response_bytes).unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error } if error.code == "operation.forbidden"
        ));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn oversize_and_malformed_envelopes_never_enter_the_actor() {
        let run_id = RunId::new();
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry
            .install(OperationControlGrant::query_only())
            .unwrap();
        let adapter = AuthenticatedOperationControlAdapter::bind_with_limit(
            profile(),
            runtime,
            registry,
            &session_id,
            32,
        )
        .unwrap();
        let oversize = adapter.handle_frame(&[b'{'; 33]).await.unwrap_err();
        assert!(matches!(oversize, AgentError::InvalidRequest(_)));
        let malformed = adapter.handle_frame(br#"{"route":"#).await.unwrap_err();
        assert!(matches!(malformed, AgentError::InvalidRequest(_)));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn decoded_json_node_budget_never_enters_the_actor() {
        let run_id = RunId::new();
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry
            .install(OperationControlGrant::query_only())
            .unwrap();
        let adapter =
            AuthenticatedOperationControlAdapter::bind(profile(), runtime, registry, &session_id)
                .unwrap();
        let mut bomb = Vec::from(&b"{\"pad\":["[..]);
        for index in 0..600 {
            if index > 0 {
                bomb.push(b',');
            }
            bomb.extend_from_slice(b"{}");
        }
        bomb.extend_from_slice(b"]}");
        assert!(bomb.len() < MAX_OPERATION_CONTROL_ENVELOPE_BYTES);
        let error = adapter.handle_frame(&bomb).await.unwrap_err();
        assert!(
            error.to_string().contains("json decode budget"),
            "control-plane DOM bombs must fail before typed projection: {error}"
        );
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn inherited_pipe_analogue_forwards_one_frame_through_the_adapter() {
        // 内存双工模拟继承匿名管道。有界毒化会话在 agent-process；
        // 这里证明适配器只吃一帧正文、盖上连接会话，且不进 actor。
        let run_id = RunId::new();
        let operation = identity(run_id);
        let (runtime, mut commands, _events) = handle(run_id);
        let registry = OperationControlSessionRegistry::new(run_id);
        let session_id = registry
            .install(OperationControlGrant::query_only())
            .unwrap();
        let adapter =
            AuthenticatedOperationControlAdapter::bind(profile(), runtime, registry, &session_id)
                .unwrap();

        let (client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await.unwrap();
            assert!(
                line.len() <= MAX_OPERATION_CONTROL_ENVELOPE_BYTES + 1,
                "oversize framing belongs to FramedProtocolSession"
            );
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            let response = adapter.handle_frame(&line).await.unwrap();
            writer.write_all(&response).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
            writer.flush().await.unwrap();
        });

        let mut forged = work(&operation);
        forged.authority_ref = Some("admin-grant".into());
        let mut request_line = serde_json::to_vec(&request(
            Route::operation_cancel(),
            forged,
            OperationCancelRequest {},
        ))
        .unwrap();
        request_line.push(b'\n');
        let (reader, mut writer) = tokio::io::split(client);
        writer.write_all(&request_line).await.unwrap();
        writer.flush().await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut response_line = Vec::new();
        reader.read_until(b'\n', &mut response_line).await.unwrap();
        if response_line.last() == Some(&b'\n') {
            response_line.pop();
        }
        let response: PlatformEnvelope<PlatformResponse<OperationCancelAck>> =
            serde_json::from_slice(&response_line).unwrap();
        assert!(matches!(
            response.payload,
            PlatformResponse::Error { error } if error.code == "operation.forbidden"
        ));
        assert_eq!(
            response.work.as_ref().unwrap().authority_ref.as_deref(),
            Some(session_id.as_str())
        );
        server_task.await.unwrap();
        assert!(commands.try_recv().is_err());
    }
}

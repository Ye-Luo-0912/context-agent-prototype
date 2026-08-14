use std::{fmt::Display, str::FromStr};

use agent_contracts::{OperationId, RunId, ScopeId, TaskId, TurnId};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::{
    ArgumentDigest, EffectId, MessageId, PlatformResponse, RequestId, SchemaDigest,
    ValidationError, ValidationResult,
    validation::{validate_identifier, validate_opaque},
};

pub const MAX_PROTOCOL_NAME_BYTES: usize = 64;
pub const MAX_ACTIVE_FEATURES: usize = 32;
pub const MAX_FEATURE_NAME_BYTES: usize = 64;
pub const MAX_ROUTE_NAMESPACE_BYTES: usize = 64;
pub const MAX_ROUTE_OPERATION_BYTES: usize = 96;
pub const MAX_CALL_ID_BYTES: usize = 256;
pub const MAX_AUTHORITY_REF_BYTES: usize = 256;
pub const MAX_TRACEPARENT_BYTES: usize = 128;
pub const MAX_TRACESTATE_BYTES: usize = 512;
pub const MAX_ATTEMPT: u16 = 32;
pub const MAX_DEADLINE_REMAINING_MS: u32 = 24 * 60 * 60 * 1_000;

pub const LIVENESS_NAMESPACE: &str = "platform";
pub const LIVENESS_OPERATION: &str = "liveness";
/// 进程能力历史纯 `ToolOutput` 响应。默认关闭，须在 ping 握手中交叉成功。
pub const FEATURE_LEGACY_INVOKE_OUTPUT: &str = "legacy.invoke-output.v1";

fn parse_canonical_contract_id<T>(value: &str) -> Result<T, String>
where
    T: FromStr + Display,
    T::Err: Display,
{
    let parsed = T::from_str(value).map_err(|error| error.to_string())?;
    if parsed.to_string() != value {
        return Err("expected the canonical lowercase hyphenated UUID form".into());
    }
    Ok(parsed)
}

macro_rules! required_contract_id_deserializer {
    ($name:ident, $ty:ty) => {
        fn $name<'de, D>(deserializer: D) -> Result<$ty, D::Error>
        where
            D: Deserializer<'de>,
        {
            let value = String::deserialize(deserializer)?;
            parse_canonical_contract_id(&value).map_err(D::Error::custom)
        }
    };
}

macro_rules! optional_contract_id_deserializer {
    ($name:ident, $ty:ty) => {
        fn $name<'de, D>(deserializer: D) -> Result<Option<$ty>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|value| parse_canonical_contract_id(&value).map_err(D::Error::custom))
                .transpose()
        }
    };
}

required_contract_id_deserializer!(deserialize_run_id, RunId);
required_contract_id_deserializer!(deserialize_operation_id, OperationId);
optional_contract_id_deserializer!(deserialize_optional_task_id, TaskId);
optional_contract_id_deserializer!(deserialize_optional_turn_id, TurnId);
optional_contract_id_deserializer!(deserialize_optional_scope_id, ScopeId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub fn validate(self) -> ValidationResult<()> {
        if self.major == 0 {
            return Err(ValidationError::new(
                "protocol.version.major",
                "must be non-zero",
            ));
        }
        Ok(())
    }
}

/// The exact, active feature set selected for this contract profile.
/// Ordering is part of the canonical wire representation: entries must be
/// strictly increasing and therefore unique.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ActiveFeatures(Vec<String>);

impl ActiveFeatures {
    pub fn new(features: Vec<String>) -> ValidationResult<Self> {
        if features.len() > MAX_ACTIVE_FEATURES {
            return Err(ValidationError::new(
                "protocol.active_features",
                format!(
                    "contains {} entries, above the {MAX_ACTIVE_FEATURES} entry bound",
                    features.len()
                ),
            ));
        }
        for feature in &features {
            validate_identifier("protocol.active_features", feature, MAX_FEATURE_NAME_BYTES)?;
        }
        if features
            .windows(2)
            .any(|pair| pair[0].as_str() >= pair[1].as_str())
        {
            return Err(ValidationError::new(
                "protocol.active_features",
                "must be strictly sorted and unique",
            ));
        }
        Ok(Self(features))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn contains(&self, feature: &str) -> bool {
        self.0
            .binary_search_by(|item| item.as_str().cmp(feature))
            .is_ok()
    }

    /// 取 `self`（本端提供）与对端声明的交集，保持提供顺序。对端不能扩充未提供的特性。
    pub fn intersect(&self, advertised: &Self) -> Self {
        Self(
            self.0
                .iter()
                .filter(|feature| advertised.contains(feature))
                .cloned()
                .collect(),
        )
    }

    /// 从握手 JSON 读特性表。缺省或 null 视为空；非法形状失败。
    pub fn from_json_value(value: Option<&serde_json::Value>) -> ValidationResult<Self> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(serde_json::Value::Array(items)) => {
                let mut features = Vec::with_capacity(items.len());
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Err(ValidationError::new(
                            "protocol.active_features",
                            "entries must be strings",
                        ));
                    };
                    features.push(name.to_owned());
                }
                Self::new(features)
            }
            Some(_) => Err(ValidationError::new(
                "protocol.active_features",
                "must be an array",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for ActiveFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let features = Vec::<String>::deserialize(deserializer)?;
        Self::new(features).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolIdentity {
    pub name: String,
    pub version: ProtocolVersion,
    pub active_features: ActiveFeatures,
    pub schema_digest: SchemaDigest,
}

impl ProtocolIdentity {
    pub fn validate(&self) -> ValidationResult<()> {
        validate_identifier("protocol.name", &self.name, MAX_PROTOCOL_NAME_BYTES)?;
        self.version.validate()
    }
}

/// An already-selected contract profile. This type validates an envelope's
/// declaration; it does not perform a handshake and a message cannot expand
/// the negotiated feature or schema set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegotiatedContractProfile {
    pub name: String,
    pub version: ProtocolVersion,
    pub active_features: ActiveFeatures,
    pub schema_digest: SchemaDigest,
}

impl NegotiatedContractProfile {
    pub fn new(
        name: impl Into<String>,
        version: ProtocolVersion,
        active_features: ActiveFeatures,
        schema_digest: SchemaDigest,
    ) -> ValidationResult<Self> {
        let profile = Self {
            name: name.into(),
            version,
            active_features,
            schema_digest,
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> ValidationResult<()> {
        validate_identifier("profile.name", &self.name, MAX_PROTOCOL_NAME_BYTES)?;
        self.version.validate()
    }

    pub fn validate_identity(&self, identity: &ProtocolIdentity) -> ValidationResult<()> {
        self.validate()?;
        identity.validate()?;
        if identity.name != self.name {
            return Err(ValidationError::new(
                "protocol.name",
                "does not match the negotiated profile",
            ));
        }
        if identity.version != self.version {
            return Err(ValidationError::new(
                "protocol.version",
                "does not match the negotiated profile",
            ));
        }
        if identity.active_features != self.active_features {
            return Err(ValidationError::new(
                "protocol.active_features",
                "do not match the negotiated profile",
            ));
        }
        if identity.schema_digest != self.schema_digest {
            return Err(ValidationError::new(
                "protocol.schema_digest",
                "does not match the negotiated profile",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeKind {
    Request,
    Response,
    Notification,
    Ping,
    Pong,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub namespace: String,
    pub operation: String,
}

impl Route {
    pub fn new(
        namespace: impl Into<String>,
        operation: impl Into<String>,
    ) -> ValidationResult<Self> {
        let route = Self {
            namespace: namespace.into(),
            operation: operation.into(),
        };
        route.validate()?;
        Ok(route)
    }

    pub fn liveness() -> Self {
        Self {
            namespace: LIVENESS_NAMESPACE.to_owned(),
            operation: LIVENESS_OPERATION.to_owned(),
        }
    }

    pub fn validate(&self) -> ValidationResult<()> {
        validate_identifier(
            "route.namespace",
            &self.namespace,
            MAX_ROUTE_NAMESPACE_BYTES,
        )?;
        validate_identifier(
            "route.operation",
            &self.operation,
            MAX_ROUTE_OPERATION_BYTES,
        )
    }

    pub fn is_liveness(&self) -> bool {
        self.namespace == LIVENESS_NAMESPACE && self.operation == LIVENESS_OPERATION
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Attempt(u16);

impl Attempt {
    pub fn new(value: u16) -> ValidationResult<Self> {
        if !(1..=MAX_ATTEMPT).contains(&value) {
            return Err(ValidationError::new(
                "work.attempt",
                format!("must be in 1..={MAX_ATTEMPT}"),
            ));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub fn checked_next(self) -> ValidationResult<Self> {
        let value = self
            .0
            .checked_add(1)
            .ok_or_else(|| ValidationError::new("work.attempt", "overflowed while advancing"))?;
        Self::new(value)
    }
}

impl Serialize for Attempt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u16(self.0)
    }
}

impl<'de> Deserialize<'de> for Attempt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u16::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeadlineRemainingMs(u32);

impl DeadlineRemainingMs {
    pub fn new(value: u32) -> ValidationResult<Self> {
        if value > MAX_DEADLINE_REMAINING_MS {
            return Err(ValidationError::new(
                "work.deadline_remaining_ms",
                format!("must not exceed {MAX_DEADLINE_REMAINING_MS} ms"),
            ));
        }
        Ok(Self(value))
    }

    pub const fn as_millis(self) -> u32 {
        self.0
    }

    /// Subtract elapsed monotonic time without ever increasing the budget.
    pub const fn after_elapsed(self, elapsed_ms: u32) -> Self {
        Self(self.0.saturating_sub(elapsed_ms))
    }

    pub const fn is_exhausted(self) -> bool {
        self.0 == 0
    }
}

impl Serialize for DeadlineRemainingMs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for DeadlineRemainingMs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Logical work identity. `task_id`, `turn_id` and `scope_id` are independent
/// optional axes; absence of one never invents an implication about another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkIdentity {
    #[serde(deserialize_with = "deserialize_run_id")]
    pub run_id: RunId,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_task_id"
    )]
    pub task_id: Option<TaskId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_turn_id"
    )]
    pub turn_id: Option<TurnId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_scope_id"
    )]
    pub scope_id: Option<ScopeId>,
    #[serde(deserialize_with = "deserialize_operation_id")]
    pub operation_id: OperationId,
    pub generation: u64,
    pub attempt: Attempt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_id: Option<EffectId>,
    pub argument_digest: ArgumentDigest,
    pub deadline_remaining_ms: DeadlineRemainingMs,
    /// Opaque reference to authority held by the trusted Platform. It is not
    /// a grant, intent, permission set, or proof of connection authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority_ref: Option<String>,
}

impl WorkIdentity {
    pub fn validate(&self) -> ValidationResult<()> {
        if self.run_id.0.is_nil()
            || self.operation_id.0.is_nil()
            || self.task_id.is_some_and(|id| id.0.is_nil())
            || self.turn_id.is_some_and(|id| id.0.is_nil())
            || self.scope_id.is_some_and(|id| id.0.is_nil())
        {
            return Err(ValidationError::new(
                "work.identity",
                "run/task/turn/scope/operation ids must not be nil UUIDs",
            ));
        }
        if let Some(call_id) = &self.call_id {
            validate_opaque("work.call_id", call_id, MAX_CALL_ID_BYTES)?;
        }
        if let Some(authority_ref) = &self.authority_ref {
            validate_opaque("work.authority_ref", authority_ref, MAX_AUTHORITY_REF_BYTES)?;
        }
        Ok(())
    }
}

/// Bounded one-hop causality. No recursive lineage or arbitrary baggage is
/// carried in the control envelope, and trace data is observability only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Causality {
    pub correlation_id: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

impl Causality {
    pub fn root(message_id: MessageId) -> Self {
        Self {
            correlation_id: message_id,
            causation_id: None,
            traceparent: None,
            tracestate: None,
        }
    }

    pub fn caused_by(correlation_id: MessageId, causation_id: MessageId) -> Self {
        Self {
            correlation_id,
            causation_id: Some(causation_id),
            traceparent: None,
            tracestate: None,
        }
    }

    pub fn validate(&self) -> ValidationResult<()> {
        if let Some(traceparent) = &self.traceparent {
            validate_opaque("causality.traceparent", traceparent, MAX_TRACEPARENT_BYTES)?;
        }
        if let Some(tracestate) = &self.tracestate {
            validate_opaque("causality.tracestate", tracestate, MAX_TRACESTATE_BYTES)?;
        }
        Ok(())
    }

    fn validate_for_message(&self, message_id: MessageId) -> ValidationResult<()> {
        self.validate()?;
        match self.causation_id {
            None if self.correlation_id != message_id => Err(ValidationError::new(
                "causality.correlation_id",
                "a root message must correlate to its own message id",
            )),
            Some(cause) if cause == message_id || self.correlation_id == message_id => {
                Err(ValidationError::new(
                    "causality.causation_id",
                    "a caused message cannot cause or root-correlate to itself",
                ))
            }
            _ => Ok(()),
        }
    }
}

/// Transport-independent envelope. Payload schemas remain typed per route;
/// this generic is not permission to build an unbounded `Value` event bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformEnvelope<P> {
    pub protocol: ProtocolIdentity,
    pub message_id: MessageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub kind: EnvelopeKind,
    pub route: Route,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkIdentity>,
    pub causality: Causality,
    pub payload: P,
}

impl<P> PlatformEnvelope<P> {
    pub fn validate(&self, profile: &NegotiatedContractProfile) -> ValidationResult<()> {
        profile.validate_identity(&self.protocol)?;
        self.route.validate()?;
        self.causality.validate_for_message(self.message_id)?;

        match self.kind {
            EnvelopeKind::Request | EnvelopeKind::Response => {
                if self.request_id.is_none() {
                    return Err(ValidationError::new(
                        "envelope.request_id",
                        "is required for a request/response",
                    ));
                }
                self.validate_work_message()?;
            }
            EnvelopeKind::Notification => {
                if self.request_id.is_some() {
                    return Err(ValidationError::new(
                        "envelope.request_id",
                        "must be absent from a notification",
                    ));
                }
                self.validate_work_message()?;
            }
            EnvelopeKind::Ping | EnvelopeKind::Pong => {
                if self.request_id.is_none() {
                    return Err(ValidationError::new(
                        "envelope.request_id",
                        "is required for liveness pairing",
                    ));
                }
                if self.work.is_some() {
                    return Err(ValidationError::new(
                        "envelope.work",
                        "liveness messages must not carry work or authority",
                    ));
                }
                if !self.route.is_liveness() {
                    return Err(ValidationError::new(
                        "envelope.route",
                        "ping/pong must use the reserved liveness route",
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_work_message(&self) -> ValidationResult<()> {
        if self.route.is_liveness() {
            return Err(ValidationError::new(
                "envelope.route",
                "the liveness route is reserved for ping/pong",
            ));
        }
        let work = self.work.as_ref().ok_or_else(|| {
            ValidationError::new("envelope.work", "is required for a work message")
        })?;
        work.validate()
    }
}

/// The entire ping/pong body. Echoing a nonce proves only liveness; it does
/// not create a session, grant authority, or advance operation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LivenessPayload {
    pub nonce: MessageId,
}

/// A forwarded deadline may only stay equal or decrease.
pub fn validate_deadline_forwarding(
    parent: DeadlineRemainingMs,
    forwarded: DeadlineRemainingMs,
) -> ValidationResult<()> {
    if forwarded > parent {
        return Err(ValidationError::new(
            "work.deadline_remaining_ms",
            "forwarding must not increase the remaining deadline",
        ));
    }
    Ok(())
}

/// Dispatch is forbidden after the remaining deadline reaches zero.
pub fn validate_dispatchable(work: &WorkIdentity) -> ValidationResult<()> {
    work.validate()?;
    if work.deadline_remaining_ms.is_exhausted() {
        return Err(ValidationError::new(
            "work.deadline_remaining_ms",
            "is exhausted; the operation must not be dispatched",
        ));
    }
    Ok(())
}

/// Validate one work response against the exact physical request it answers.
pub fn validate_response_pair<RequestPayload, ResponsePayload>(
    profile: &NegotiatedContractProfile,
    request: &PlatformEnvelope<RequestPayload>,
    response: &PlatformEnvelope<PlatformResponse<ResponsePayload>>,
) -> ValidationResult<()> {
    request.validate(profile)?;
    response.validate(profile)?;
    if request.kind != EnvelopeKind::Request || response.kind != EnvelopeKind::Response {
        return Err(ValidationError::new(
            "envelope.kind",
            "response pairing requires request then response",
        ));
    }
    validate_common_pair(request, response)?;
    let request_work = request.work.as_ref().expect("validated work request");
    let response_work = response.work.as_ref().expect("validated work response");
    validate_same_attempt(request_work, response_work)?;
    response.payload.validate()?;
    if let PlatformResponse::Error { error } = &response.payload
        && let Some(retry_after) = error.retry_after_ms
        && retry_after > response_work.deadline_remaining_ms
    {
        return Err(ValidationError::new(
            "error.retry_after_ms",
            "must not exceed the response's remaining deadline",
        ));
    }
    validate_deadline_forwarding(
        request_work.deadline_remaining_ms,
        response_work.deadline_remaining_ms,
    )
}

/// Validate an application-level logical retry. Transport redelivery of an
/// unchanged encoded message is not a retry and therefore does not use this
/// validator.
pub fn validate_retry<PreviousPayload, RetryPayload>(
    profile: &NegotiatedContractProfile,
    previous: &PlatformEnvelope<PreviousPayload>,
    retry: &PlatformEnvelope<RetryPayload>,
) -> ValidationResult<()> {
    previous.validate(profile)?;
    retry.validate(profile)?;
    if previous.kind != EnvelopeKind::Request || retry.kind != EnvelopeKind::Request {
        return Err(ValidationError::new(
            "envelope.kind",
            "logical retry requires two request envelopes",
        ));
    }
    if previous.protocol != retry.protocol || previous.route != retry.route {
        return Err(ValidationError::new(
            "envelope.route",
            "a retry must preserve protocol identity and route",
        ));
    }
    if previous.message_id == retry.message_id {
        return Err(ValidationError::new(
            "envelope.message_id",
            "an application-level retry requires a new message id",
        ));
    }
    if previous.request_id == retry.request_id {
        return Err(ValidationError::new(
            "envelope.request_id",
            "an application-level retry requires a new physical request id",
        ));
    }
    validate_causality_pair(previous, retry)?;

    let previous_work = previous.work.as_ref().expect("validated work request");
    let retry_work = retry.work.as_ref().expect("validated retry request");
    validate_same_logical_work(previous_work, retry_work)?;
    if retry_work.attempt != previous_work.attempt.checked_next()? {
        return Err(ValidationError::new(
            "work.attempt",
            "a retry must advance attempt by exactly one",
        ));
    }
    validate_deadline_forwarding(
        previous_work.deadline_remaining_ms,
        retry_work.deadline_remaining_ms,
    )
}

/// Validate a root liveness request. The correlation is self-rooted and no
/// operation/task/lease state is admitted.
pub fn validate_ping(
    profile: &NegotiatedContractProfile,
    ping: &PlatformEnvelope<LivenessPayload>,
) -> ValidationResult<()> {
    ping.validate(profile)?;
    if ping.kind != EnvelopeKind::Ping {
        return Err(ValidationError::new("envelope.kind", "expected a ping"));
    }
    if ping.causality.correlation_id != ping.message_id || ping.causality.causation_id.is_some() {
        return Err(ValidationError::new(
            "envelope.causality",
            "ping must be a self-correlated root message",
        ));
    }
    Ok(())
}

pub fn validate_pong_pair(
    profile: &NegotiatedContractProfile,
    ping: &PlatformEnvelope<LivenessPayload>,
    pong: &PlatformEnvelope<LivenessPayload>,
) -> ValidationResult<()> {
    validate_ping(profile, ping)?;
    pong.validate(profile)?;
    if pong.kind != EnvelopeKind::Pong {
        return Err(ValidationError::new("envelope.kind", "expected a pong"));
    }
    validate_common_pair(ping, pong)?;
    if pong.payload.nonce != ping.payload.nonce {
        return Err(ValidationError::new(
            "liveness.nonce",
            "pong must echo the ping nonce",
        ));
    }
    Ok(())
}

fn validate_common_pair<RequestPayload, ResponsePayload>(
    request: &PlatformEnvelope<RequestPayload>,
    response: &PlatformEnvelope<ResponsePayload>,
) -> ValidationResult<()> {
    if request.protocol != response.protocol || request.route != response.route {
        return Err(ValidationError::new(
            "envelope.route",
            "paired messages must preserve protocol identity and route",
        ));
    }
    if request.request_id != response.request_id {
        return Err(ValidationError::new(
            "envelope.request_id",
            "response does not answer this physical request",
        ));
    }
    if request.message_id == response.message_id {
        return Err(ValidationError::new(
            "envelope.message_id",
            "request and response must have distinct message ids",
        ));
    }
    validate_causality_pair(request, response)
}

fn validate_causality_pair<ParentPayload, ChildPayload>(
    parent: &PlatformEnvelope<ParentPayload>,
    child: &PlatformEnvelope<ChildPayload>,
) -> ValidationResult<()> {
    if child.causality.correlation_id != parent.causality.correlation_id
        || child.causality.causation_id != Some(parent.message_id)
    {
        return Err(ValidationError::new(
            "envelope.causality",
            "child must preserve correlation and name the parent message as its one-hop cause",
        ));
    }
    Ok(())
}

fn validate_same_attempt(left: &WorkIdentity, right: &WorkIdentity) -> ValidationResult<()> {
    validate_same_logical_work(left, right)?;
    if left.attempt != right.attempt {
        return Err(ValidationError::new(
            "work.attempt",
            "a response must preserve the request attempt",
        ));
    }
    Ok(())
}

fn validate_same_logical_work(left: &WorkIdentity, right: &WorkIdentity) -> ValidationResult<()> {
    if left.run_id != right.run_id
        || left.task_id != right.task_id
        || left.turn_id != right.turn_id
        || left.scope_id != right.scope_id
        || left.operation_id != right.operation_id
        || left.generation != right.generation
        || left.call_id != right.call_id
        || left.effect_id != right.effect_id
        || left.argument_digest != right.argument_digest
        || left.authority_ref != right.authority_ref
    {
        return Err(ValidationError::new(
            "work.identity",
            "logical work identity changed across the exchange/retry",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use agent_contracts::{OperationId, RunId, ScopeId, TaskId, TurnId};
    use serde_json::json;

    use super::*;
    use crate::{
        EffectStateDisposition, MAX_DIAGNOSTIC_REF_BYTES, MAX_ERROR_CODE_BYTES,
        MAX_ERROR_MESSAGE_BYTES, PlatformError, PlatformErrorClass, RetryDisposition,
    };

    const MESSAGE_1: &str = "00000000-0000-4000-8000-000000000001";
    const MESSAGE_2: &str = "00000000-0000-4000-8000-000000000002";
    const MESSAGE_3: &str = "00000000-0000-4000-8000-000000000003";
    const REQUEST_1: &str = "00000000-0000-4000-8000-000000000011";
    const REQUEST_2: &str = "00000000-0000-4000-8000-000000000012";
    const EFFECT_1: &str = "00000000-0000-4000-8000-000000000021";
    const RUN: &str = "00000000-0000-4000-8000-000000000031";
    const TASK: &str = "00000000-0000-4000-8000-000000000032";
    const TURN: &str = "00000000-0000-4000-8000-000000000033";
    const SCOPE: &str = "00000000-0000-4000-8000-000000000034";
    const OPERATION: &str = "00000000-0000-4000-8000-000000000035";

    fn protocol() -> ProtocolIdentity {
        ProtocolIdentity {
            name: "focus-agent.platform".into(),
            version: ProtocolVersion { major: 1, minor: 0 },
            active_features: ActiveFeatures::new(vec!["effects.v1".into(), "tools.v1".into()])
                .unwrap(),
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

    fn work(attempt: u16, deadline_ms: u32) -> WorkIdentity {
        WorkIdentity {
            run_id: RunId::from_str(RUN).unwrap(),
            task_id: Some(TaskId::from_str(TASK).unwrap()),
            turn_id: Some(TurnId::from_str(TURN).unwrap()),
            scope_id: Some(ScopeId::from_str(SCOPE).unwrap()),
            operation_id: OperationId::from_str(OPERATION).unwrap(),
            generation: 7,
            attempt: Attempt::new(attempt).unwrap(),
            call_id: Some("call_model_1".into()),
            effect_id: Some(EffectId::from_str(EFFECT_1).unwrap()),
            argument_digest: ArgumentDigest::from_bytes([0x22; 32]),
            deadline_remaining_ms: DeadlineRemainingMs::new(deadline_ms).unwrap(),
            authority_ref: Some("lease-opaque-1".into()),
        }
    }

    fn work_request() -> PlatformEnvelope<serde_json::Value> {
        let message_id = MessageId::from_str(MESSAGE_1).unwrap();
        PlatformEnvelope {
            protocol: protocol(),
            message_id,
            request_id: Some(RequestId::from_str(REQUEST_1).unwrap()),
            kind: EnvelopeKind::Request,
            route: Route::new("tool", "invoke").unwrap(),
            work: Some(work(1, 30_000)),
            causality: Causality::root(message_id),
            payload: json!({"path": "src/lib.rs"}),
        }
    }

    fn response_for(
        request: &PlatformEnvelope<serde_json::Value>,
    ) -> PlatformEnvelope<PlatformResponse<serde_json::Value>> {
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
            payload: PlatformResponse::Success {
                value: json!({"ok": true}),
            },
        }
    }

    #[test]
    fn request_has_a_stable_serde_golden_shape() {
        let encoded = serde_json::to_string(&work_request()).unwrap();
        let digest_11 = "11".repeat(32);
        let digest_22 = "22".repeat(32);
        let expected = format!(
            "{{\"protocol\":{{\"name\":\"focus-agent.platform\",\"version\":{{\"major\":1,\"minor\":0}},\"active_features\":[\"effects.v1\",\"tools.v1\"],\"schema_digest\":\"{digest_11}\"}},\"message_id\":\"{MESSAGE_1}\",\"request_id\":\"{REQUEST_1}\",\"kind\":\"request\",\"route\":{{\"namespace\":\"tool\",\"operation\":\"invoke\"}},\"work\":{{\"run_id\":\"{RUN}\",\"task_id\":\"{TASK}\",\"turn_id\":\"{TURN}\",\"scope_id\":\"{SCOPE}\",\"operation_id\":\"{OPERATION}\",\"generation\":7,\"attempt\":1,\"call_id\":\"call_model_1\",\"effect_id\":\"{EFFECT_1}\",\"argument_digest\":\"{digest_22}\",\"deadline_remaining_ms\":30000,\"authority_ref\":\"lease-opaque-1\"}},\"causality\":{{\"correlation_id\":\"{MESSAGE_1}\"}},\"payload\":{{\"path\":\"src/lib.rs\"}}}}"
        );
        assert_eq!(encoded, expected);
        let decoded: PlatformEnvelope<serde_json::Value> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, work_request());
        decoded.validate(&profile()).unwrap();
    }

    #[test]
    fn digest_is_fixed_lower_hex_sha256_of_exact_bytes() {
        let digest = ArgumentDigest::sha256_bytes(b"abc");
        assert_eq!(
            digest.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let round_trip: ArgumentDigest = serde_json::from_str(&format!("\"{digest}\"")).unwrap();
        assert_eq!(round_trip, digest);
        assert!(
            serde_json::from_str::<ArgumentDigest>(&format!(
                "\"{}\"",
                digest.to_string().to_uppercase()
            ))
            .is_err()
        );
        assert!(serde_json::from_str::<ArgumentDigest>("\"abcd\"").is_err());

        // Raw byte hashes remain order-sensitive; semantic identity uses JCS.
        assert_ne!(
            ArgumentDigest::sha256_bytes(br#"{"a":1,"b":2}"#),
            ArgumentDigest::sha256_bytes(br#"{"b":2,"a":1}"#)
        );
        assert_eq!(
            ArgumentDigest::from_json(&serde_json::json!({"a":1,"b":2})),
            ArgumentDigest::from_json(&serde_json::json!({"b":2,"a":1}))
        );
    }

    #[test]
    fn active_features_are_bounded_sorted_and_unique() {
        assert!(ActiveFeatures::new(vec!["a.v1".into(), "b.v1".into()]).is_ok());
        assert!(ActiveFeatures::new(vec!["b.v1".into(), "a.v1".into()]).is_err());
        assert!(ActiveFeatures::new(vec!["a.v1".into(), "a.v1".into()]).is_err());
        assert!(ActiveFeatures::new(vec!["Upper".into()]).is_err());
        assert!(ActiveFeatures::new(vec!["a".repeat(MAX_FEATURE_NAME_BYTES + 1)]).is_err());
        assert!(ActiveFeatures::new(vec!["a".into(); MAX_ACTIVE_FEATURES + 1]).is_err());
        assert!(serde_json::from_str::<ActiveFeatures>(r#"["b","a"]"#).is_err());
        let offered =
            ActiveFeatures::new(vec![FEATURE_LEGACY_INVOKE_OUTPUT.into(), "tools.v1".into()])
                .unwrap();
        let advertised =
            ActiveFeatures::from_json_value(Some(&json!([FEATURE_LEGACY_INVOKE_OUTPUT]))).unwrap();
        assert!(
            offered
                .intersect(&advertised)
                .contains(FEATURE_LEGACY_INVOKE_OUTPUT)
        );
        assert!(!offered.intersect(&advertised).contains("tools.v1"));
        assert!(ActiveFeatures::from_json_value(None).unwrap().is_empty());
    }

    #[test]
    fn negotiated_profile_rejects_version_feature_and_schema_drift() {
        let profile = profile();
        let mut identity = protocol();
        identity.version.minor += 1;
        assert!(profile.validate_identity(&identity).is_err());

        let mut identity = protocol();
        identity.active_features = ActiveFeatures::new(vec!["effects.v1".into()]).unwrap();
        assert!(profile.validate_identity(&identity).is_err());

        let mut identity = protocol();
        identity.schema_digest = SchemaDigest::from_bytes([0x33; 32]);
        assert!(profile.validate_identity(&identity).is_err());
    }

    #[test]
    fn response_pair_pins_physical_and_logical_identity() {
        let request = work_request();
        let response = response_for(&request);
        validate_response_pair(&profile(), &request, &response).unwrap();

        let mut wrong_request = response.clone();
        wrong_request.request_id = Some(RequestId::from_str(REQUEST_2).unwrap());
        assert!(validate_response_pair(&profile(), &request, &wrong_request).is_err());

        let mut wrong_work = response.clone();
        wrong_work.work.as_mut().unwrap().effect_id = None;
        assert!(validate_response_pair(&profile(), &request, &wrong_work).is_err());

        let mut increased_deadline = response;
        increased_deadline
            .work
            .as_mut()
            .unwrap()
            .deadline_remaining_ms = DeadlineRemainingMs::new(30_001).unwrap();
        assert!(validate_response_pair(&profile(), &request, &increased_deadline).is_err());
    }

    #[test]
    fn retry_changes_delivery_but_preserves_logical_work() {
        let previous = work_request();
        let mut retry = previous.clone();
        retry.message_id = MessageId::from_str(MESSAGE_2).unwrap();
        retry.request_id = Some(RequestId::from_str(REQUEST_2).unwrap());
        retry.causality =
            Causality::caused_by(previous.causality.correlation_id, previous.message_id);
        let retry_work = retry.work.as_mut().unwrap();
        retry_work.attempt = Attempt::new(2).unwrap();
        retry_work.deadline_remaining_ms = DeadlineRemainingMs::new(20_000).unwrap();
        validate_retry(&profile(), &previous, &retry).unwrap();

        let mut same_delivery = retry.clone();
        same_delivery.request_id = previous.request_id;
        assert!(validate_retry(&profile(), &previous, &same_delivery).is_err());

        let mut changed_arguments = retry.clone();
        changed_arguments.work.as_mut().unwrap().argument_digest =
            ArgumentDigest::from_bytes([0x44; 32]);
        assert!(validate_retry(&profile(), &previous, &changed_arguments).is_err());

        let mut skipped_attempt = retry;
        skipped_attempt.work.as_mut().unwrap().attempt = Attempt::new(3).unwrap();
        assert!(validate_retry(&profile(), &previous, &skipped_attempt).is_err());
    }

    #[test]
    fn deadline_is_monotonic_and_zero_is_not_dispatchable() {
        let parent = DeadlineRemainingMs::new(1_000).unwrap();
        let forwarded = parent.after_elapsed(250);
        assert_eq!(forwarded.as_millis(), 750);
        validate_deadline_forwarding(parent, forwarded).unwrap();
        assert!(
            validate_deadline_forwarding(parent, DeadlineRemainingMs::new(1_001).unwrap()).is_err()
        );

        let exhausted = work(1, 0);
        assert!(exhausted.validate().is_ok());
        assert!(validate_dispatchable(&exhausted).is_err());
        assert!(DeadlineRemainingMs::new(MAX_DEADLINE_REMAINING_MS + 1).is_err());
        assert!(Attempt::new(0).is_err());
        assert!(Attempt::new(MAX_ATTEMPT + 1).is_err());
    }

    #[test]
    fn ping_and_pong_are_stateless_liveness_only() {
        let ping_message = MessageId::from_str(MESSAGE_1).unwrap();
        let ping = PlatformEnvelope {
            protocol: protocol(),
            message_id: ping_message,
            request_id: Some(RequestId::from_str(REQUEST_1).unwrap()),
            kind: EnvelopeKind::Ping,
            route: Route::liveness(),
            work: None,
            causality: Causality::root(ping_message),
            payload: LivenessPayload {
                nonce: MessageId::from_str(MESSAGE_3).unwrap(),
            },
        };
        validate_ping(&profile(), &ping).unwrap();

        let pong = PlatformEnvelope {
            protocol: ping.protocol.clone(),
            message_id: MessageId::from_str(MESSAGE_2).unwrap(),
            request_id: ping.request_id,
            kind: EnvelopeKind::Pong,
            route: Route::liveness(),
            work: None,
            causality: Causality::caused_by(ping.causality.correlation_id, ping.message_id),
            payload: ping.payload,
        };
        validate_pong_pair(&profile(), &ping, &pong).unwrap();

        let mut authority_smuggling = ping.clone();
        authority_smuggling.work = Some(work(1, 1_000));
        assert!(validate_ping(&profile(), &authority_smuggling).is_err());

        let mut wrong_nonce = pong;
        wrong_nonce.payload.nonce = MessageId::new();
        assert!(validate_pong_pair(&profile(), &ping, &wrong_nonce).is_err());
    }

    #[test]
    fn structured_error_dispositions_reject_blind_effect_retry() {
        let valid = PlatformError {
            class: PlatformErrorClass::Domain,
            code: "effect.outcome_unknown".into(),
            message: "connection closed after commit started".into(),
            retry: RetryDisposition::QueryBeforeRetry,
            effect_state: EffectStateDisposition::OutcomeUnknown,
            retry_after_ms: None,
            diagnostic_ref: Some("artifact://run/diagnostic.json".into()),
        };
        valid.validate().unwrap();

        let mut unknowable = valid.clone();
        unknowable.retry = RetryDisposition::Never;
        unknowable.validate().unwrap();

        let mut blind_retry = valid.clone();
        blind_retry.retry = RetryDisposition::SameOperation;
        assert!(blind_retry.validate().is_err());

        let protocol_error = PlatformError {
            class: PlatformErrorClass::Protocol,
            code: "protocol.schema_mismatch".into(),
            message: "schema mismatch".into(),
            retry: RetryDisposition::Never,
            effect_state: EffectStateDisposition::NotApplicable,
            retry_after_ms: None,
            diagnostic_ref: None,
        };
        protocol_error.validate().unwrap();

        // Error class describes where the failure was detected, not whether
        // a previously dispatched effect is known to have applied. A broken
        // response frame after dispatch can therefore be a protocol error
        // whose effect outcome must be queried before retry.
        let mut ambiguous_protocol_error = protocol_error;
        ambiguous_protocol_error.code = "protocol.response_lost".into();
        ambiguous_protocol_error.retry = RetryDisposition::QueryBeforeRetry;
        ambiguous_protocol_error.effect_state = EffectStateDisposition::OutcomeUnknown;
        ambiguous_protocol_error.validate().unwrap();

        let retryable = PlatformError {
            class: PlatformErrorClass::Domain,
            code: "provider.busy".into(),
            message: "busy".into(),
            retry: RetryDisposition::SameOperation,
            effect_state: EffectStateDisposition::NotApplied,
            retry_after_ms: Some(DeadlineRemainingMs::new(500).unwrap()),
            diagnostic_ref: None,
        };
        retryable.validate().unwrap();

        let mut applied_retry = retryable;
        applied_retry.effect_state = EffectStateDisposition::Applied;
        assert!(applied_retry.validate().is_err());
    }

    #[test]
    fn response_carrier_is_explicit_bounded_and_pair_validated() {
        let request = work_request();
        let success = response_for(&request);
        validate_response_pair(&profile(), &request, &success).unwrap();
        assert_eq!(
            serde_json::to_value(&success.payload).unwrap(),
            json!({"status": "success", "value": {"ok": true}})
        );

        let mut failure = response_for(&request);
        failure.payload = PlatformResponse::Error {
            error: PlatformError {
                class: PlatformErrorClass::Domain,
                code: "effect.outcome_unknown".into(),
                message: "response was lost after dispatch".into(),
                retry: RetryDisposition::QueryBeforeRetry,
                effect_state: EffectStateDisposition::OutcomeUnknown,
                retry_after_ms: None,
                diagnostic_ref: None,
            },
        };
        validate_response_pair(&profile(), &request, &failure).unwrap();
        assert_eq!(
            serde_json::to_value(&failure.payload).unwrap()["status"],
            "error"
        );

        if let PlatformResponse::Error { error } = &mut failure.payload {
            error.retry = RetryDisposition::SameOperation;
        }
        assert!(validate_response_pair(&profile(), &request, &failure).is_err());

        if let PlatformResponse::Error { error } = &mut failure.payload {
            error.effect_state = EffectStateDisposition::NotApplied;
            error.retry_after_ms = Some(DeadlineRemainingMs::new(30_000).unwrap());
        }
        assert!(validate_response_pair(&profile(), &request, &failure).is_err());

        assert!(
            serde_json::from_value::<PlatformResponse<serde_json::Value>>(json!({
                "status": "success",
                "value": null,
                "error": {"code": "ambiguous"}
            }))
            .is_err()
        );
    }

    #[test]
    fn core_envelope_fields_and_causality_fail_closed() {
        let request = work_request();
        let mut encoded = serde_json::to_value(&request).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .insert("future_core_field".into(), json!(true));
        assert!(serde_json::from_value::<PlatformEnvelope<serde_json::Value>>(encoded).is_err());

        let mut wrong_root = request.clone();
        wrong_root.causality.correlation_id = MessageId::from_str(MESSAGE_2).unwrap();
        assert!(wrong_root.validate(&profile()).is_err());

        let mut self_caused = request;
        self_caused.causality.causation_id = Some(self_caused.message_id);
        assert!(self_caused.validate(&profile()).is_err());

        assert!(MessageId::from_str("00000000-0000-0000-0000-000000000000").is_err());
        assert!(MessageId::from_str("00000000000040008000000000000001").is_err());
        assert!(MessageId::from_str("00000000-0000-4000-8000-000000000001").is_ok());
    }

    #[test]
    fn every_free_string_family_has_a_hard_bound() {
        let mut request = work_request();
        request.protocol.name = "a".repeat(MAX_PROTOCOL_NAME_BYTES + 1);
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.route.operation = "a".repeat(MAX_ROUTE_OPERATION_BYTES + 1);
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.work.as_mut().unwrap().call_id = Some("x".repeat(MAX_CALL_ID_BYTES + 1));
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.work.as_mut().unwrap().authority_ref =
            Some("x".repeat(MAX_AUTHORITY_REF_BYTES + 1));
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.causality.traceparent = Some("x".repeat(MAX_TRACEPARENT_BYTES + 1));
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.causality.tracestate = Some("x".repeat(MAX_TRACESTATE_BYTES + 1));
        assert!(request.validate(&profile()).is_err());

        for (code, message, diagnostic_ref) in [
            ("a".repeat(MAX_ERROR_CODE_BYTES + 1), "ok".into(), None),
            (
                "valid.code".into(),
                "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
                None,
            ),
            (
                "valid.code".into(),
                "ok".into(),
                Some("x".repeat(MAX_DIAGNOSTIC_REF_BYTES + 1)),
            ),
        ] {
            let error = PlatformError {
                class: PlatformErrorClass::Domain,
                code,
                message,
                retry: RetryDisposition::Never,
                effect_state: EffectStateDisposition::NotApplicable,
                retry_after_ms: None,
                diagnostic_ref,
            };
            assert!(error.validate().is_err());
        }
    }

    #[test]
    fn envelope_shape_rejects_missing_work_and_misplaced_request_ids() {
        let mut request = work_request();
        request.work = None;
        assert!(request.validate(&profile()).is_err());

        let mut request = work_request();
        request.request_id = None;
        assert!(request.validate(&profile()).is_err());

        let mut notification = work_request();
        notification.kind = EnvelopeKind::Notification;
        assert!(notification.validate(&profile()).is_err());
        notification.request_id = None;
        notification.validate(&profile()).unwrap();
    }
}

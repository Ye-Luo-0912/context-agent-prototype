//! Transport-independent Platform *work-control* routing (P1/P2).
//!
//! The run-scoped sibling of [`super::OperationControlRouter`]: submit /
//! continue / cancel / snapshot / subscribe / approval-respond, each routed
//! through the sole actor over [`RuntimeHandle`]. Session authorization is
//! installed by the trusted composition root; wire strings never mint or
//! widen a grant. Submission receipts come from the atomic `start_work`
//! command, and the typed snapshot is assembled in one serialized actor step.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_contracts::{AgentError, AgentResult, RunId, RuntimeEventEnvelope, ToolRisk};
use agent_core::{ApprovalBroker, InteractiveApprovalGate};
use agent_platform_protocol::{
    ApprovalRespondOutcome, ApprovalRespondRequest, ApprovalRespondResponse, ApprovalRisk,
    EffectStateDisposition, EnvelopeKind, FocusSnapshot, MAX_SNAPSHOT_PENDING_APPROVALS,
    MAX_SNAPSHOT_TASKS, MessageId, NegotiatedContractProfile, PendingApprovalSnapshot,
    PlatformEnvelope, PlatformError, PlatformErrorClass, PlatformResponse, RetryDisposition,
    TaskSnapshotEntry, TaskSnapshotStatus, ValidationError, ValidationResult, WorkCancelRequest,
    WorkCancelResponse, WorkContinueRequest, WorkContinueResponse, WorkSnapshotRequest,
    WorkSnapshotResponse, WorkSubmitDisposition, WorkSubmitRequest, WorkSubmitResponse,
    WorkSubscribeRequest, WorkSubscribeResponse, validate_approval_respond_request,
    validate_approval_respond_response, validate_work_cancel_request,
    validate_work_cancel_response, validate_work_continue_request, validate_work_continue_response,
    validate_work_snapshot_request, validate_work_snapshot_response, validate_work_submit_request,
    validate_work_submit_response, validate_work_subscribe_request,
    validate_work_subscribe_response,
};
use tokio::sync::broadcast;

use super::platform_error;
use crate::RuntimeHandle;

/// Bounded control deadline for run-scoped requests (they carry no work
/// deadline). Generous enough for a serialized actor queue, hard enough that
/// a stuck actor cannot hang a connection forever.
pub const WORK_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Simultaneous installed work-control sessions.
pub const MAX_WORK_CONTROL_SESSIONS: usize = 64;
/// Session-id byte bound (same shape as the operation-control registry).
pub const MAX_WORK_SESSION_ID_BYTES: usize = 256;

fn closed_work_stream() -> broadcast::Receiver<RuntimeEventEnvelope> {
    let (_, receiver) = broadcast::channel(1);
    receiver
}

/// Permission being requested from trusted, connection-scoped policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkControlAction {
    Submit,
    Continue,
    Cancel,
    Snapshot,
    Subscribe,
    ApprovalRespond,
}

/// Bounded facts supplied to the trusted authorizer. `authority_ref` is only
/// an opaque lookup key; possession of the string does not grant authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkControlAuthorizationRequest {
    pub action: WorkControlAction,
    pub run_id: RunId,
    pub authority_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkControlAuthorization {
    Authorized,
    Denied,
}

/// Resolves an already-authenticated session/grant at the Platform boundary.
/// Implementations belong to trusted composition; wire peers cannot mint one.
pub trait WorkControlAuthorizer: Send + Sync {
    fn authorize(&self, request: &WorkControlAuthorizationRequest) -> WorkControlAuthorization;
}

/// One installed session's allowed work-control actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkControlGrant {
    pub allow_submit: bool,
    pub allow_continue: bool,
    pub allow_cancel: bool,
    pub allow_snapshot: bool,
    pub allow_subscribe: bool,
    pub allow_approval_respond: bool,
}

impl WorkControlGrant {
    /// Observe-only: snapshot and subscribe, no mutation, no approval voice.
    pub const fn read_only() -> Self {
        Self {
            allow_submit: false,
            allow_continue: false,
            allow_cancel: false,
            allow_snapshot: true,
            allow_subscribe: true,
            allow_approval_respond: false,
        }
    }

    /// Full operator control over this run's work plane.
    pub const fn operator() -> Self {
        Self {
            allow_submit: true,
            allow_continue: true,
            allow_cancel: true,
            allow_snapshot: true,
            allow_subscribe: true,
            allow_approval_respond: true,
        }
    }

    fn allows(self, action: WorkControlAction) -> bool {
        match action {
            WorkControlAction::Submit => self.allow_submit,
            WorkControlAction::Continue => self.allow_continue,
            WorkControlAction::Cancel => self.allow_cancel,
            WorkControlAction::Snapshot => self.allow_snapshot,
            WorkControlAction::Subscribe => self.allow_subscribe,
            WorkControlAction::ApprovalRespond => self.allow_approval_respond,
        }
    }
}

/// Trusted session table, mirroring the operation-control registry. Peers
/// cannot insert records; revocation takes effect immediately.
pub struct WorkControlSessionRegistry {
    run_id: RunId,
    sessions: Mutex<std::collections::HashMap<String, WorkControlGrant>>,
}

impl WorkControlSessionRegistry {
    pub fn new(run_id: RunId) -> Arc<Self> {
        Arc::new(Self {
            run_id,
            sessions: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn install(&self, grant: WorkControlGrant) -> AgentResult<String> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.len() >= MAX_WORK_CONTROL_SESSIONS {
            return Err(AgentError::InvalidRequest(format!(
                "work-control session registry is limited to {MAX_WORK_CONTROL_SESSIONS} live grants"
            )));
        }
        let session_id = agent_contracts::OperationId::new().to_string();
        sessions.insert(session_id.clone(), grant);
        Ok(session_id)
    }

    pub fn revoke(&self, session_id: &str) -> AgentResult<()> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.remove(session_id).is_none() {
            return Err(AgentError::InvalidRequest(
                "work-control session is not installed".into(),
            ));
        }
        Ok(())
    }

    /// How many session grants are installed right now. Supervision/test
    /// observation of the table, not an authority surface: a long-lived host
    /// must return to its baseline once connections disconnect, and this is
    /// the fact that proves it.
    pub fn live_sessions(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .len()
    }

    pub fn bind(self: &Arc<Self>, session_id: &str) -> AgentResult<BoundWorkSessionAuthorizer> {
        if session_id.is_empty() || session_id.len() > MAX_WORK_SESSION_ID_BYTES {
            return Err(AgentError::InvalidRequest(
                "work-control session id is out of bounds".into(),
            ));
        }
        {
            let sessions = self.sessions.lock().expect("session registry poisoned");
            if !sessions.contains_key(session_id) {
                return Err(AgentError::InvalidRequest(
                    "work-control session is not installed".into(),
                ));
            }
        }
        Ok(BoundWorkSessionAuthorizer {
            run_id: self.run_id,
            session_id: session_id.to_owned(),
            registry: Arc::clone(self),
        })
    }
}

/// One connection's authorizer: looks up the installed session only; the
/// wire `authority_ref` is ignored.
pub struct BoundWorkSessionAuthorizer {
    run_id: RunId,
    session_id: String,
    registry: Arc<WorkControlSessionRegistry>,
}

impl BoundWorkSessionAuthorizer {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl WorkControlAuthorizer for BoundWorkSessionAuthorizer {
    fn authorize(&self, request: &WorkControlAuthorizationRequest) -> WorkControlAuthorization {
        if request.run_id != self.run_id {
            return WorkControlAuthorization::Denied;
        }
        let sessions = self
            .registry
            .sessions
            .lock()
            .expect("session registry poisoned");
        match sessions.get(&self.session_id) {
            Some(grant) if grant.allows(request.action) => WorkControlAuthorization::Authorized,
            _ => WorkControlAuthorization::Denied,
        }
    }
}

/// Authorized, transport-independent work-control router: a facade over the
/// actor command channel plus the approval plane, not a scheduler or a
/// second authority. Named Pipe, UDS or in-process adapters can all reuse it.
pub struct WorkControlRouter {
    profile: NegotiatedContractProfile,
    runtime: RuntimeHandle,
    broker: Arc<ApprovalBroker>,
    gate: Arc<InteractiveApprovalGate>,
    authorizer: Arc<dyn WorkControlAuthorizer>,
}

type ResponseValidator<RequestPayload, ResponsePayload> = fn(
    &NegotiatedContractProfile,
    &PlatformEnvelope<RequestPayload>,
    &PlatformEnvelope<PlatformResponse<ResponsePayload>>,
) -> ValidationResult<()>;

impl WorkControlRouter {
    pub fn new(
        profile: NegotiatedContractProfile,
        runtime: RuntimeHandle,
        broker: Arc<ApprovalBroker>,
        gate: Arc<InteractiveApprovalGate>,
        authorizer: Arc<dyn WorkControlAuthorizer>,
    ) -> AgentResult<Self> {
        profile
            .validate()
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        Ok(Self {
            profile,
            runtime,
            broker,
            gate,
            authorizer,
        })
    }

    /// The live event stream, for subscribers. The receiver sees events from
    /// this point on; lag is reported by the channel as an explicit error,
    /// never silently skipped.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.runtime.subscribe()
    }

    /// Atomic long-task submission (P1): one actor command for focus,
    /// checklist attach and the first input; the receipt carries the focused
    /// task. A repeated id with identical content returns `AlreadyAccepted`
    /// without re-execution.
    pub async fn submit(
        &self,
        request: PlatformEnvelope<WorkSubmitRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>> {
        validate_work_submit_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::Submit, &request) {
            return self.submit_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }
        let submission = self
            .bounded(started, || {
                self.runtime.start_work(
                    request.payload.goal.clone(),
                    request.payload.client_request_id.clone(),
                )
            })
            .await;
        match submission {
            Ok(submission) => {
                let disposition = match submission.disposition {
                    crate::WorkSubmissionDisposition::Accepted => WorkSubmitDisposition::Accepted,
                    crate::WorkSubmissionDisposition::AlreadyAccepted => {
                        WorkSubmitDisposition::AlreadyAccepted
                    }
                };
                let value = WorkSubmitResponse {
                    disposition,
                    task_id: submission.task_id,
                };
                self.submit_response(&request, started, PlatformResponse::Success { value })
            }
            Err(error) => self.submit_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: work_runtime_error(error),
                },
            ),
        }
    }

    /// Continue the active task's stored directive. The response carries the
    /// task the directive continues under, read atomically with the turn
    /// start so no second client can switch focus in between.
    pub async fn continue_work(
        &self,
        request: PlatformEnvelope<WorkContinueRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkContinueResponse>>> {
        validate_work_continue_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::Continue, &request) {
            return self.continue_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }
        let continued = self
            .bounded(started, || self.runtime.continue_active_task())
            .await;
        match continued {
            Ok(task_id) => self.continue_response(
                &request,
                started,
                PlatformResponse::Success {
                    value: WorkContinueResponse { task_id },
                },
            ),
            Err(error) => self.continue_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: work_runtime_error(error),
                },
            ),
        }
    }

    /// Cancel the current in-flight turn. The acknowledgement is Core's
    /// exact truth (`Cancelled` proves the durable barrier); `NoActiveTurn`
    /// is a fact, not an error.
    pub async fn cancel(
        &self,
        request: PlatformEnvelope<WorkCancelRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkCancelResponse>>> {
        validate_work_cancel_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::Cancel, &request) {
            return self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }
        let ack = self.bounded(started, || self.runtime.cancel_turn()).await;
        match ack {
            Ok(ack) => self.cancel_response(
                &request,
                started,
                PlatformResponse::Success {
                    value: WorkCancelResponse { ack },
                },
            ),
            Err(error) => self.cancel_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: work_runtime_error(error),
                },
            ),
        }
    }

    /// Actor-consistent task/focus snapshot, followed by the Core approval
    /// plane's current pending set. Approval responses recheck that set at
    /// delivery. Both reads share one bounded request deadline.
    pub async fn snapshot(
        &self,
        request: PlatformEnvelope<WorkSnapshotRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>> {
        validate_work_snapshot_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::Snapshot, &request) {
            return self.snapshot_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }
        let status = self
            .bounded(started, || self.runtime.status_snapshot())
            .await;
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                return self.snapshot_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: work_runtime_error(error),
                    },
                );
            }
        };
        let tasks: Vec<TaskSnapshotEntry> = status
            .tasks
            .iter()
            .filter(|task| !matches!(task.status, crate::TaskStatus::Completed))
            .take(MAX_SNAPSHOT_TASKS)
            .map(|task| TaskSnapshotEntry {
                task_id: task.id,
                goal: task.goal.clone(),
                status: match task.status {
                    crate::TaskStatus::Active => TaskSnapshotStatus::Active,
                    crate::TaskStatus::Suspended => TaskSnapshotStatus::Suspended,
                    crate::TaskStatus::Completed => TaskSnapshotStatus::Completed,
                },
                anchor_revision: task.anchor_revision,
                tool_requirement_revision: task.tool_requirement_revision,
                tool_requirement_count: u32::try_from(task.tool_requirement_count)
                    .unwrap_or(u32::MAX),
            })
            .collect();
        let focus = status.focus_task_id.map(|task_id| FocusSnapshot {
            task_id,
            goal: status.focus_goal.clone(),
            anchor_revision: status.focus_anchor_revision,
        });
        let pending = match self
            .bounded(started, || async { Ok(self.broker.pending().await) })
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                return self.snapshot_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: work_runtime_error(error),
                    },
                );
            }
        };
        let pending_approvals: Vec<PendingApprovalSnapshot> = pending
            .into_iter()
            .take(MAX_SNAPSHOT_PENDING_APPROVALS)
            .map(|pending| PendingApprovalSnapshot {
                request_id: pending.request_id,
                call_name: pending.call.name.clone(),
                risk: approval_risk(pending.spec.risk),
                target_summary: approval_target_summary(&pending.call.arguments),
            })
            .collect();
        let value = WorkSnapshotResponse {
            run_started: status.serving,
            run_completed: false,
            watermark: status.watermark,
            focus,
            tasks,
            pending_approvals,
            // Stream gaps are reported by the subscribe handshake or the
            // broadcast channel's explicit lag error.
            resync_required: false,
        };
        self.snapshot_response(&request, started, PlatformResponse::Success { value })
    }

    /// Subscribe handshake: returns the watermark the live stream starts
    /// from plus the receiver. This router has no event replay: any cursor
    /// differing from the serialized watermark requires a fresh snapshot.
    /// Register before the barrier so events after its watermark cannot fall
    /// between the snapshot and receiver creation. Consumers discard queued
    /// events at or below the snapshot/handshake watermark.
    pub async fn subscribe(
        &self,
        request: PlatformEnvelope<WorkSubscribeRequest>,
    ) -> AgentResult<(
        PlatformEnvelope<PlatformResponse<WorkSubscribeResponse>>,
        broadcast::Receiver<RuntimeEventEnvelope>,
    )> {
        validate_work_subscribe_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::Subscribe, &request) {
            let response = self.subscribe_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            )?;
            return Ok((response, closed_work_stream()));
        }
        let stream = self.subscribe_events();
        let status = match self
            .bounded(started, || self.runtime.status_snapshot())
            .await
        {
            Ok(status) => status,
            Err(error) => {
                let response = self.subscribe_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: work_runtime_error(error),
                    },
                )?;
                return Ok((response, closed_work_stream()));
            }
        };
        let watermark = status.watermark;
        let resync_required = request
            .payload
            .replay_after_seq
            .is_some_and(|cursor| cursor != watermark);
        let response = self.subscribe_response(
            &request,
            started,
            PlatformResponse::Success {
                value: WorkSubscribeResponse {
                    watermark,
                    resync_required,
                },
            },
        )?;
        Ok((response, stream))
    }

    /// Respond to one pending approval. The decision rides the installed
    /// session; a late or duplicate response returns `NoLongerPending` — the
    /// current fact, not a failure and not a second delivery.
    pub async fn respond(
        &self,
        request: PlatformEnvelope<ApprovalRespondRequest>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<ApprovalRespondResponse>>> {
        validate_approval_respond_request(&self.profile, &request)
            .map_err(|error| AgentError::InvalidRequest(error.to_string()))?;
        let started = Instant::now();
        if !self.is_authorized(WorkControlAction::ApprovalRespond, &request) {
            return self.respond_response(
                &request,
                started,
                PlatformResponse::Error {
                    error: forbidden_error(),
                },
            );
        }
        let delivered = match self
            .bounded(started, || {
                let gate = Arc::clone(&self.gate);
                let request_id = request.payload.request_id.clone();
                let decision = request.payload.decision;
                async move {
                    let outcome = gate.respond(&request_id, decision).await;
                    Ok::<_, AgentError>(outcome)
                }
            })
            .await
        {
            Ok(delivered) => delivered,
            Err(error) => {
                return self.respond_response(
                    &request,
                    started,
                    PlatformResponse::Error {
                        error: work_runtime_error(error),
                    },
                );
            }
        };
        let outcome = if delivered {
            ApprovalRespondOutcome::Delivered
        } else {
            ApprovalRespondOutcome::NoLongerPending
        };
        self.respond_response(
            &request,
            started,
            PlatformResponse::Success {
                value: ApprovalRespondResponse { outcome },
            },
        )
    }

    async fn bounded<T, F>(&self, started: Instant, call: impl FnOnce() -> F) -> AgentResult<T>
    where
        F: Future<Output = AgentResult<T>>,
    {
        let remaining = WORK_CONTROL_TIMEOUT.saturating_sub(started.elapsed());
        match tokio::time::timeout(remaining, call()).await {
            Ok(result) => result,
            Err(_) => Err(AgentError::Transport {
                retryable: false,
                message: "work-control request exceeded its bounded deadline".into(),
            }),
        }
    }

    fn is_authorized<P>(&self, action: WorkControlAction, request: &PlatformEnvelope<P>) -> bool {
        self.authorizer.authorize(&WorkControlAuthorizationRequest {
            action,
            run_id: self.runtime.run_id(),
            authority_ref: request
                .work
                .as_ref()
                .and_then(|work| work.authority_ref.clone()),
        }) == WorkControlAuthorization::Authorized
    }

    fn submit_response(
        &self,
        request: &PlatformEnvelope<WorkSubmitRequest>,
        started: Instant,
        payload: PlatformResponse<WorkSubmitResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkSubmitResponse>>> {
        self.finish(request, started, payload, validate_work_submit_response)
    }

    fn continue_response(
        &self,
        request: &PlatformEnvelope<WorkContinueRequest>,
        started: Instant,
        payload: PlatformResponse<WorkContinueResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkContinueResponse>>> {
        self.finish(request, started, payload, validate_work_continue_response)
    }

    fn cancel_response(
        &self,
        request: &PlatformEnvelope<WorkCancelRequest>,
        started: Instant,
        payload: PlatformResponse<WorkCancelResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkCancelResponse>>> {
        self.finish(request, started, payload, validate_work_cancel_response)
    }

    fn snapshot_response(
        &self,
        request: &PlatformEnvelope<WorkSnapshotRequest>,
        started: Instant,
        payload: PlatformResponse<WorkSnapshotResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkSnapshotResponse>>> {
        self.finish(request, started, payload, validate_work_snapshot_response)
    }

    fn subscribe_response(
        &self,
        request: &PlatformEnvelope<WorkSubscribeRequest>,
        started: Instant,
        payload: PlatformResponse<WorkSubscribeResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<WorkSubscribeResponse>>> {
        self.finish(request, started, payload, validate_work_subscribe_response)
    }

    fn respond_response(
        &self,
        request: &PlatformEnvelope<ApprovalRespondRequest>,
        started: Instant,
        payload: PlatformResponse<ApprovalRespondResponse>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<ApprovalRespondResponse>>> {
        self.finish(
            request,
            started,
            payload,
            validate_approval_respond_response,
        )
    }

    fn finish<RequestPayload, ResponsePayload>(
        &self,
        request: &PlatformEnvelope<RequestPayload>,
        _started: Instant,
        payload: PlatformResponse<ResponsePayload>,
        validate: ResponseValidator<RequestPayload, ResponsePayload>,
    ) -> AgentResult<PlatformEnvelope<PlatformResponse<ResponsePayload>>> {
        let response = run_scoped_response_envelope(request, payload);
        validate(&self.profile, request, &response)
            .map_err(|error: ValidationError| AgentError::Internal(error.to_string()))?;
        Ok(response)
    }
}

/// F12: the gate's own declared risk, mirrored into the protocol's tag.
fn approval_risk(risk: ToolRisk) -> ApprovalRisk {
    match risk {
        ToolRisk::ReadOnly => ApprovalRisk::ReadOnly,
        ToolRisk::WorkspaceWrite => ApprovalRisk::WorkspaceWrite,
        ToolRisk::ProcessExecution => ApprovalRisk::ProcessExecution,
    }
}

/// F12: a bounded operator-facing summary of what one pending approval
/// targets (workspace path or argv/command). This is a DISPLAY projection of
/// the call's own structured arguments, read from the same well-known keys
/// the host policy table binds; it is never parsed from tool prose, never an
/// authority surface, and the approval gate's own intent matching stays the
/// sole authority. `None` when the arguments carry none of the keys — the
/// UI then shows "unavailable" instead of guessing.
fn approval_target_summary(arguments: &serde_json::Value) -> Option<String> {
    let mut paths: Vec<&str> = Vec::new();
    if let Some(path) = string_argument(arguments, "path") {
        paths.push(path);
    }
    if let Some(files) = arguments.get("files").and_then(serde_json::Value::as_array) {
        for file in files {
            if let Some(path) = file.get("path").and_then(serde_json::Value::as_str) {
                let path = path.trim();
                if !path.is_empty() {
                    paths.push(path);
                }
            }
        }
    }
    if !paths.is_empty() {
        return bounded_display_summary(join_bounded(&paths, " → "));
    }
    if let Some(command) = string_argument(arguments, "command") {
        return bounded_display_summary(command.to_owned());
    }
    if let Some(argv) = arguments.get("argv").and_then(serde_json::Value::as_array) {
        let tokens: Vec<&str> = argv
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .collect();
        if !tokens.is_empty() {
            return bounded_display_summary(join_bounded(&tokens, " "));
        }
    }
    None
}

fn string_argument<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// At most three entries survive; the rest stay counted, not hidden.
fn join_bounded(entries: &[&str], separator: &str) -> String {
    const MAX_ENTRIES: usize = 3;
    match entries.len() {
        0 => String::new(),
        1..=MAX_ENTRIES => entries.join(separator),
        _ => format!(
            "{}{separator}+{} more",
            entries[..MAX_ENTRIES].join(separator),
            entries.len() - MAX_ENTRIES
        ),
    }
}

/// Sanitize, bound, and only then expose: control characters become spaces
/// (a display line, not a transcript), the char bound holds with an honest
/// truncation marker inside it, and an empty result means "not available"
/// instead of an empty claim.
fn bounded_display_summary(summary: String) -> Option<String> {
    let max = agent_platform_protocol::MAX_SNAPSHOT_APPROVAL_TARGET_CHARS;
    let mut bounded = summary.trim().to_owned();
    if bounded.chars().any(char::is_control) {
        bounded = bounded
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
    }
    if bounded.chars().count() > max {
        bounded = bounded.chars().take(max - 1).collect();
        bounded.push('…');
    }
    let bounded = bounded.trim().to_owned();
    if bounded.is_empty() {
        return None;
    }
    Some(bounded)
}

fn run_scoped_response_envelope<RequestPayload, ResponsePayload>(
    request: &PlatformEnvelope<RequestPayload>,
    payload: PlatformResponse<ResponsePayload>,
) -> PlatformEnvelope<PlatformResponse<ResponsePayload>> {
    // Deadline bookkeeping lives in the transport for run-scoped routes;
    // keeping the envelope work-free is part of the contract.
    PlatformEnvelope {
        protocol: request.protocol.clone(),
        message_id: MessageId::new(),
        request_id: request.request_id,
        kind: EnvelopeKind::Response,
        route: request.route.clone(),
        work: None,
        causality: agent_platform_protocol::Causality::caused_by(
            request.causality.correlation_id,
            request.message_id,
        ),
        payload,
    }
}

fn forbidden_error() -> PlatformError {
    platform_error(
        PlatformErrorClass::Domain,
        "work.forbidden",
        "work control is not permitted under the installed Platform session",
        RetryDisposition::Never,
        EffectStateDisposition::NotApplicable,
    )
}

fn work_runtime_error(error: AgentError) -> PlatformError {
    match error {
        AgentError::RecoveryRequired(_) => platform_error(
            PlatformErrorClass::Domain,
            "work.recovery_required",
            "Core or Runtime authority is recovery-fenced; query the snapshot before retry",
            RetryDisposition::QueryBeforeRetry,
            EffectStateDisposition::OutcomeUnknown,
        ),
        AgentError::InvalidRequest(message) => platform_error(
            PlatformErrorClass::Domain,
            "work.rejected",
            &message,
            RetryDisposition::Never,
            EffectStateDisposition::NotApplied,
        ),
        AgentError::Transport { message, .. } => platform_error(
            PlatformErrorClass::Domain,
            "work.deadline_exceeded",
            &message,
            RetryDisposition::QueryBeforeRetry,
            EffectStateDisposition::OutcomeUnknown,
        ),
        error => platform_error(
            PlatformErrorClass::Domain,
            "work.control_unavailable",
            &error.to_string(),
            RetryDisposition::Never,
            EffectStateDisposition::NotApplicable,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkSubmissionDisposition;
    use agent_platform_protocol::Route;

    #[test]
    fn runtime_error_cases_cover_receipt_semantics() {
        // A rejected submit (conflict or busy) must never look retryable.
        let rejected = work_runtime_error(AgentError::InvalidRequest(
            "client request id was already admitted for a different goal".into(),
        ));
        assert_eq!(rejected.code, "work.rejected");
        assert!(!matches!(rejected.retry, RetryDisposition::SameOperation));
        let recovered = work_runtime_error(AgentError::RecoveryRequired("fenced".into()));
        assert_eq!(recovered.code, "work.recovery_required");
        let _ = WorkSubmissionDisposition::Accepted;
        let _ = Route::work_submit();
    }

    #[test]
    fn session_registry_count_reflects_install_and_revoke() {
        let registry = WorkControlSessionRegistry::new(RunId::new());
        assert_eq!(registry.live_sessions(), 0);
        let first = registry.install(WorkControlGrant::operator()).unwrap();
        let second = registry.install(WorkControlGrant::read_only()).unwrap();
        assert_eq!(registry.live_sessions(), 2);
        assert!(registry.revoke(&first).is_ok());
        assert_eq!(registry.live_sessions(), 1);
        assert!(registry.revoke(&second).is_ok());
        assert_eq!(registry.live_sessions(), 0);
    }

    /// F12: the target summary is a bounded display projection of the call's
    /// own structured arguments, in the gate's key vocabulary — never parsed
    /// from prose, never authority.
    #[test]
    fn approval_target_summary_projects_the_well_known_argument_keys() {
        // Workspace write: the path argument.
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "path": "docs/plan.md",
                "content": "…"
            }))
            .as_deref(),
            Some("docs/plan.md")
        );

        // Multi-file patch: every target stays visible, bounded to three
        // entries plus an honest "+n more".
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "files": [
                    {"path": "a.rs", "hunks": []},
                    {"path": "b.rs", "hunks": []},
                    {"path": "c.rs", "hunks": []},
                    {"path": "d.rs", "hunks": []}
                ]
            }))
            .as_deref(),
            Some("a.rs → b.rs → c.rs → +1 more")
        );

        // Shell command.
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "command": "cargo test --workspace",
                "dialect": "sh"
            }))
            .as_deref(),
            Some("cargo test --workspace")
        );

        // Process argv.
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "argv": ["cargo", "build", "--release"]
            }))
            .as_deref(),
            Some("cargo build --release")
        );

        // None of the well-known keys: honestly unavailable, not guessed.
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "question": "what should I run?"
            })),
            None
        );
        assert_eq!(approval_target_summary(&serde_json::json!({})), None);
    }

    #[test]
    fn approval_target_summary_is_sanitized_and_bounded() {
        // Control characters never reach the wire as controls.
        assert_eq!(
            approval_target_summary(&serde_json::json!({
                "path": "bad\u{1}path"
            }))
            .as_deref(),
            Some("bad path")
        );

        // An overlong target stays inside the protocol bound (the truncation
        // marker included) and still validates as snapshot text.
        let long_path =
            "d".repeat(agent_platform_protocol::MAX_SNAPSHOT_APPROVAL_TARGET_CHARS + 64);
        let summary = approval_target_summary(&serde_json::json!({ "path": long_path }))
            .expect("overlong path still produces a bounded summary");
        assert!(
            summary.chars().count() <= agent_platform_protocol::MAX_SNAPSHOT_APPROVAL_TARGET_CHARS
        );
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn approval_risk_mirrors_the_gate_declaration() {
        assert_eq!(approval_risk(ToolRisk::ReadOnly), ApprovalRisk::ReadOnly);
        assert_eq!(
            approval_risk(ToolRisk::WorkspaceWrite),
            ApprovalRisk::WorkspaceWrite
        );
        assert_eq!(
            approval_risk(ToolRisk::ProcessExecution),
            ApprovalRisk::ProcessExecution
        );
    }
}

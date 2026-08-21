use std::{
    collections::{HashMap, VecDeque},
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ApprovalGate, CancellationToken, EffectIntent,
    IntentShadowGate, ShadowVerdict, StandingGrant, ToolCall, ToolRisk, ToolSpec,
};
use tokio::sync::{Mutex, broadcast, oneshot};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PolicyApprovalGate {
    pub allow_workspace_write: bool,
    pub allow_process_execution: bool,
}

impl PolicyApprovalGate {
    pub fn read_only() -> Self {
        Self {
            allow_workspace_write: false,
            allow_process_execution: false,
        }
    }

    pub fn permissive() -> Self {
        Self {
            allow_workspace_write: true,
            allow_process_execution: true,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalGate for PolicyApprovalGate {
    async fn authorize(
        &self,
        _call: &ToolCall,
        spec: &ToolSpec,
        _cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        let allowed = match spec.risk {
            ToolRisk::ReadOnly => true,
            ToolRisk::WorkspaceWrite => self.allow_workspace_write,
            ToolRisk::ProcessExecution => self.allow_process_execution,
        };
        Ok(if allowed {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        })
    }
}

/// A pending interactive approval request, broadcast to UI subscribers.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub call: ToolCall,
    pub spec: ToolSpec,
}

/// Shared hub between the `InteractiveApprovalGate` (kernel side) and the UI.
/// The UI subscribes to `subscribe()` for new requests and answers them with
/// `respond()`. Late subscribers can drain `pending()`.
pub struct ApprovalBroker {
    pending: Arc<Mutex<VecDeque<ApprovalRequest>>>,
    notify: broadcast::Sender<ApprovalRequest>,
}

impl ApprovalBroker {
    pub fn new() -> Arc<Self> {
        let (notify, _) = broadcast::channel(128);
        Arc::new(Self {
            pending: Arc::new(Mutex::new(VecDeque::new())),
            notify,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalRequest> {
        self.notify.subscribe()
    }

    pub async fn pending(&self) -> Vec<ApprovalRequest> {
        self.pending.lock().await.iter().cloned().collect()
    }

    async fn push(&self, request: ApprovalRequest) {
        let _ = self.notify.send(request.clone());
        self.pending.lock().await.push_back(request);
    }

    async fn remove(&self, request_id: &str) {
        let mut guard = self.pending.lock().await;
        guard.retain(|request| request.request_id != request_id);
    }
}

/// Prompts the user for every workspace-write / process-execution call by
/// broadcasting an `ApprovalRequest` and waiting for the UI's decision.
///
/// The wait is bounded by `answer_timeout` (default 5 minutes) so a missing
/// responder can never hang a turn forever; on timeout the call is denied.
pub struct InteractiveApprovalGate {
    broker: Arc<ApprovalBroker>,
    decisions: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    answer_timeout: Duration,
}

impl InteractiveApprovalGate {
    pub fn new(broker: Arc<ApprovalBroker>) -> Self {
        Self {
            broker,
            decisions: Mutex::new(HashMap::new()),
            answer_timeout: Duration::from_secs(300),
        }
    }

    /// Override how long a request may wait for the UI's answer.
    pub fn with_answer_timeout(mut self, timeout: Duration) -> Self {
        self.answer_timeout = timeout;
        self
    }
}

impl InteractiveApprovalGate {
    /// Resolve a pending request from the UI side. Returns false if the
    /// request is unknown or already answered.
    pub async fn respond(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        let sender = self.decisions.lock().await.remove(request_id);
        let Some(sender) = sender else {
            return false;
        };
        self.broker.remove(request_id).await;
        let _ = sender.send(decision);
        true
    }
}

#[async_trait::async_trait]
impl ApprovalGate for InteractiveApprovalGate {
    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        if spec.risk == ToolRisk::ReadOnly {
            return Ok(ApprovalDecision::Allow);
        }

        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.decisions.lock().await.insert(request_id.clone(), tx);

        self.broker
            .push(ApprovalRequest {
                request_id: request_id.clone(),
                call: call.clone(),
                spec: spec.clone(),
            })
            .await;

        tokio::select! {
            decision = rx => {
                // The responder answered (and already removed the pending
                // entries); a dropped sender without a decision surfaces as
                // a denial — never as a silent allow.
                self.broker.remove(&request_id).await;
                Ok(decision.map_err(|_| {
                    AgentError::ApprovalDenied("approval request dropped (no responder)".into())
                })?)
            }
            _ = cancel.cancelled() => {
                // The operation this approval belonged to was cancelled:
                // stop waiting immediately and remove every pending entry —
                // a cancelled turn must not leave an unanswered request in
                // the broker or the decisions map.
                self.decisions.lock().await.remove(&request_id);
                self.broker.remove(&request_id).await;
                Err(AgentError::Cancelled)
            }
            _ = tokio::time::sleep(self.answer_timeout) => {
                self.decisions.lock().await.remove(&request_id);
                self.broker.remove(&request_id).await;
                Err(AgentError::ApprovalDenied(
                    "approval request timed out (no response)".into(),
                ))
            }
        }
    }
}

/// How many decision entries the bounded audit keeps (observability, not a
/// policy surface).
const GRANT_AUDIT_CAP: usize = 64;

/// A granted decision record for observability: which call was decided by a
/// standing grant (and its id) versus delegated to the underlying gate.
#[derive(Debug, Clone)]
pub struct GrantAuditEntry {
    pub call_name: String,
    pub risk: ToolRisk,
    /// The grant id when a standing grant decided the call; `None` when the
    /// call was delegated to the underlying gate.
    pub grant_id: Option<String>,
}

/// Composes a trusted standing-grant layer over any underlying
/// `ApprovalGate` (interactive prompts or policy booleans).
///
/// A call that matches an active standing grant is allowed immediately —
/// no per-call prompt — which is what lets a long coding task edit its
/// granted workspace and run bounded local tests unattended. A call that
/// matches nothing falls through to the underlying gate (which denies on a
/// missing responder), so zero user responses can never expand privileges
/// beyond the granted envelope and never stall a turn for the underlying
/// answer timeout when a grant covers the operation.
///
/// The model can use a matching grant but cannot widen it: grants are only
/// established through `grant` (composition root / UI), only shrink
/// (revocation, run consumption, expiry) and never grant beyond their
/// declared target. The final effect of a granted workspace write is still
/// bounded by the confined workspace (CORE-07) and the runtime's generation
/// fence — the grant is an approval decision, not a sandbox bypass.
pub struct TaskApprovalGate {
    inner: Arc<dyn ApprovalGate>,
    grants: Mutex<HashMap<String, GrantEntry>>,
    audit: Mutex<VecDeque<GrantAuditEntry>>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
}

struct GrantEntry {
    grant: StandingGrant,
    runs_used: u32,
}

impl TaskApprovalGate {
    pub fn new(inner: Arc<dyn ApprovalGate>) -> Self {
        Self::with_clock(inner, Box::new(now_ms))
    }

    /// Test seam: an injectable clock so expiry behavior is deterministic.
    pub fn with_clock(
        inner: Arc<dyn ApprovalGate>,
        now: Box<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            grants: Mutex::new(HashMap::new()),
            audit: Mutex::new(VecDeque::new()),
            now,
        }
    }

    /// Establish a standing grant. The grant is validated (target scope is
    /// clean and inside the workspace, risk is a real effect class, not
    /// already expired) before it can match anything.
    pub async fn grant(&self, grant: StandingGrant) -> AgentResult<()> {
        if grant.id.is_empty() {
            return Err(AgentError::InvalidRequest(
                "grant id must not be empty".into(),
            ));
        }
        match grant.risk {
            ToolRisk::ReadOnly => {
                return Err(AgentError::InvalidRequest(
                    "standing grants cover write/process effects, not read-only calls".into(),
                ));
            }
            ToolRisk::WorkspaceWrite => {
                let Some(prefix) = &grant.target.workspace_path_prefix else {
                    return Err(AgentError::InvalidRequest(
                        "a workspace-write grant needs a workspace_path_prefix target".into(),
                    ));
                };
                if relative_components(prefix).is_none_or(|parts| parts.is_empty()) {
                    return Err(AgentError::InvalidRequest(format!(
                        "grant target must be a clean non-root workspace-relative path: {prefix}"
                    )));
                }
            }
            ToolRisk::ProcessExecution => {
                let exec = grant
                    .target
                    .exec_argv_prefix
                    .as_ref()
                    .filter(|tokens| tokens.iter().any(|token| !token.trim().is_empty()));
                let shell = grant
                    .target
                    .shell_command_digest
                    .as_ref()
                    .map(|digest| digest.trim())
                    .filter(|digest| !digest.is_empty());
                match (exec, shell) {
                    (Some(_), None) | (None, Some(_)) => {}
                    (Some(_), Some(_)) => {
                        return Err(AgentError::InvalidRequest(
                            "a process grant cannot mix exec_argv_prefix and shell_command_digest"
                                .into(),
                        ));
                    }
                    (None, None) => {
                        return Err(AgentError::InvalidRequest(
                            "a process grant needs exec_argv_prefix or shell_command_digest".into(),
                        ));
                    }
                }
            }
        }
        if grant.expires_at_ms <= (self.now)() {
            return Err(AgentError::InvalidRequest(
                "grant is already expired".into(),
            ));
        }
        self.grants.lock().await.insert(
            grant.id.clone(),
            GrantEntry {
                grant,
                runs_used: 0,
            },
        );
        Ok(())
    }

    /// Revoke a grant by id; returns whether one was live.
    pub async fn revoke(&self, id: &str) -> bool {
        self.grants.lock().await.remove(id).is_some()
    }

    /// The live grants (expired entries are pruned first).
    pub async fn active_grants(&self) -> Vec<StandingGrant> {
        let now = (self.now)();
        let mut book = self.grants.lock().await;
        book.retain(|_, entry| entry.grant.expires_at_ms > now);
        book.values().map(|entry| entry.grant.clone()).collect()
    }

    /// Bounded audit of recent decisions: granted (with the grant id) or
    /// delegated to the underlying gate.
    pub async fn recent_decisions(&self) -> Vec<GrantAuditEntry> {
        self.audit.lock().await.iter().cloned().collect()
    }

    /// Whether the derived intent of `call` falls inside the grant's
    /// declared target scope. Matching is effect-derived: the grant is
    /// compared against the concrete intent (path, content size, argv prefix
    /// or exact shell digest), never against the tool name alone.
    fn grant_matches(grant: &StandingGrant, intent: &EffectIntent) -> bool {
        if grant.risk != intent.risk() {
            return false;
        }
        match intent {
            EffectIntent::ReadOnly => false,
            EffectIntent::WorkspaceWrite {
                path,
                content_bytes,
            } => {
                let Some(prefix) = &grant.target.workspace_path_prefix else {
                    return false;
                };
                if !path_within_prefix(path, prefix) {
                    return false;
                }
                // The write's content is part of the effect scope: a grant
                // with a content cap does not cover oversized writes.
                if let Some(max) = grant.constraint.max_content_bytes
                    && *content_bytes > max
                {
                    return false;
                }
                true
            }
            EffectIntent::WorkspaceWriteSet { writes } => {
                let Some(prefix) = &grant.target.workspace_path_prefix else {
                    return false;
                };
                // Every write target must be inside the grant's prefix:
                // the first file matching a grant must never widen
                // authority to the remaining files of the same call
                // (MOD-AUTH-01). An empty set fails closed, and each
                // entry's byte estimate plus the total must respect a
                // content cap.
                if writes.is_empty()
                    || !writes
                        .iter()
                        .all(|bound| path_within_prefix(&bound.path, prefix))
                {
                    return false;
                }
                if let Some(max) = grant.constraint.max_content_bytes {
                    let total: u64 = writes
                        .iter()
                        .map(|bound| bound.max_bytes)
                        .fold(0u64, u64::saturating_add);
                    if total > max || writes.iter().any(|bound| bound.max_bytes > max) {
                        return false;
                    }
                }
                true
            }
            EffectIntent::ExecArgv { program, argv } => {
                let Some(prefix) = grant.target.exec_argv_prefix.as_ref() else {
                    return false;
                };
                agent_contracts::exec_argv_intent(prefix).covers(&EffectIntent::ExecArgv {
                    program: program.clone(),
                    argv: argv.clone(),
                })
            }
            EffectIntent::ShellExec { command_digest, .. } => {
                let Some(approved) = grant.target.shell_command_digest.as_ref() else {
                    return false;
                };
                !approved.is_empty() && approved == command_digest
            }
        }
    }

    /// Derive the concrete effect intent of one call from its validated
    /// arguments. Shared with the v2 lease path through the contracts-level
    /// `derive_effect_intent` so approval and lease minting can never
    /// drift; missing or malformed arguments produce the empty intent of
    /// that class (an empty path/command can never match a grant), which
    /// is the same fail-closed behavior as the legacy argument parsing.
    fn derive_effect_intent(call: &ToolCall, spec: &ToolSpec) -> EffectIntent {
        agent_contracts::derive_effect_intent(call, spec)
    }

    async fn record(&self, call_name: &str, risk: ToolRisk, grant_id: Option<String>) {
        let mut audit = self.audit.lock().await;
        if audit.len() >= GRANT_AUDIT_CAP {
            audit.pop_front();
        }
        audit.push_back(GrantAuditEntry {
            call_name: call_name.to_string(),
            risk,
            grant_id,
        });
    }
}

/// A human-readable label of a derived intent, for shadow verdict reasons.
fn intent_label(intent: &EffectIntent) -> String {
    match intent {
        EffectIntent::ReadOnly => "read-only".to_string(),
        EffectIntent::WorkspaceWrite { path, .. } => {
            format!("workspace write to '{path}'")
        }
        EffectIntent::WorkspaceWriteSet { writes, .. } => {
            format!(
                "workspace write set [{}]",
                writes
                    .iter()
                    .map(|bound| bound.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        EffectIntent::ExecArgv { program, argv } => {
            format!("exec {program} {argv:?}")
        }
        EffectIntent::ShellExec {
            dialect,
            command_digest,
        } => format!("shell exec dialect='{dialect}' digest={command_digest}"),
    }
}

/// The shadow mode of the standing-grant gate: the v2 deny-by-default
/// perspective, recorded beside the legacy path and never enforced. The
/// verdict reuses the *same* matching logic as `authorize` (derived intent
/// against live grants, including the run cap), but does not consume any
/// state — so the invariant "shadow `Granted` implies legacy `Allow`" holds
/// by construction, and an ungranted write/process call is `Denied` here
/// even when the legacy inner gate (permissive policy, interactive prompt)
/// would allow it. That asymmetry — shadow stricter than legacy — is the
/// point of shadow mode; the reverse would be a privilege-expansion bug.
#[async_trait::async_trait]
impl IntentShadowGate for TaskApprovalGate {
    async fn shadow_verdict(&self, call: &ToolCall, spec: &ToolSpec) -> ShadowVerdict {
        if spec.risk == ToolRisk::ReadOnly {
            return ShadowVerdict::Granted {
                grant_id: "read-only".into(),
                reason: "read-only calls need no standing grant".into(),
            };
        }
        let intent = Self::derive_effect_intent(call, spec);
        if matches!(intent, EffectIntent::ReadOnly) {
            return ShadowVerdict::Granted {
                grant_id: "session-control".into(),
                reason: "process.session poll/stop do not spawn a new command".into(),
            };
        }
        let now = (self.now)();
        let mut book = self.grants.lock().await;
        book.retain(|_, entry| entry.grant.expires_at_ms > now);
        for (id, entry) in book.iter() {
            if !Self::grant_matches(&entry.grant, &intent) {
                continue;
            }
            // An exhausted grant must not grant in shadow either: the
            // legacy path refuses it, so granting here would make the
            // shadow gate look *wider* than the legacy gate.
            if let Some(max) = entry.grant.constraint.max_runs
                && entry.runs_used >= max
            {
                continue;
            }
            return ShadowVerdict::Granted {
                grant_id: id.clone(),
                reason: format!(
                    "derived intent ({}) matches standing grant '{}'",
                    intent_label(&intent),
                    id
                ),
            };
        }
        ShadowVerdict::Denied {
            reason: format!(
                "no live standing grant matches the derived intent ({})",
                intent_label(&intent)
            ),
        }
    }
}

#[async_trait::async_trait]
impl ApprovalGate for TaskApprovalGate {
    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        if spec.risk == ToolRisk::ReadOnly {
            return Ok(ApprovalDecision::Allow);
        }

        let now = (self.now)();
        let mut book = self.grants.lock().await;
        // Prune expired grants first, so an expired grant never matches and
        // `active_grants` stays accurate.
        book.retain(|_, entry| entry.grant.expires_at_ms > now);

        // Approval is effect-derived: derive the concrete intent from the
        // validated arguments and match grants against it.
        let intent = Self::derive_effect_intent(call, spec);
        // `process.session` poll/stop keep ProcessExecution dispatch
        // identity but do not spawn. They must not consume an argv-prefix
        // or shell-digest grant and must not fall through as an empty
        // ExecArgv.
        if matches!(intent, EffectIntent::ReadOnly) {
            drop(book);
            self.record(&call.name, spec.risk, None).await;
            return Ok(ApprovalDecision::Allow);
        }

        let mut matched_id: Option<String> = None;
        for (id, entry) in book.iter_mut() {
            if !Self::grant_matches(&entry.grant, &intent) {
                continue;
            }
            if let Some(max) = entry.grant.constraint.max_runs
                && entry.runs_used >= max
            {
                continue;
            }
            matched_id = Some(id.clone());
            if matches!(
                intent,
                EffectIntent::ExecArgv { .. } | EffectIntent::ShellExec { .. }
            ) {
                entry.runs_used += 1;
            }
            break;
        }
        drop(book);

        if let Some(id) = matched_id {
            self.record(&call.name, spec.risk, Some(id)).await;
            return Ok(ApprovalDecision::Allow);
        }

        self.record(&call.name, spec.risk, None).await;
        self.inner.authorize(call, spec, cancel).await
    }
}

/// Split a workspace-relative path into clean components. Returns `None`
/// for absolute paths, parent escapes and root/drive prefixes — such a path
/// can never match a grant.
fn relative_components(path: &str) -> Option<Vec<String>> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts)
}

/// Component-aware prefix match: `path` is inside `prefix` when the clean
/// path components start with the clean prefix components. `src/` covers
/// `src/main.rs` but neither `src-other/x` nor a bare `src` file write.
fn path_within_prefix(path: &str, prefix: &str) -> bool {
    let (Some(path_parts), Some(prefix_parts)) =
        (relative_components(path), relative_components(prefix))
    else {
        return false;
    };
    if path_parts.len() < prefix_parts.len() {
        return false;
    }
    path_parts[..prefix_parts.len()] == prefix_parts[..]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{EffectIntent, GrantTarget, ToolRisk, ToolSemanticRole, ToolSpec};
    use serde_json::json;

    fn write_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "fs.write".into(),
            arguments: json!({"path": "x.txt", "content": "x"}),
        }
    }

    fn write_spec() -> ToolSpec {
        ToolSpec {
            name: "fs.write".into(),
            description: "write a file".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }
    }

    /// Wait until the request is visible in the broker and return its id.
    async fn wait_for_request(broker: &ApprovalBroker) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(request) = broker.pending().await.into_iter().next() {
                return request.request_id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the approval request never reached the broker"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn cancelled_approval_cleans_up_pending_entries() {
        // A cancelled operation must not leave its approval request behind:
        // both the broker entry and the decisions entry are removed, and
        // the wait ends immediately instead of running out the answer
        // timeout.
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let gate_for_task = gate.clone();
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();

        let handle = tokio::spawn(async move {
            gate_for_task
                .authorize(&write_call(), &write_spec(), &cancel_for_task)
                .await
        });

        let request_id = wait_for_request(&broker).await;
        assert!(
            !broker.pending().await.is_empty(),
            "a request must be pending while the UI has not answered"
        );

        cancel.cancel();
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(AgentError::Cancelled)),
            "a cancelled approval must report cancellation: {result:?}"
        );

        assert!(
            broker.pending().await.is_empty(),
            "no pending request may remain after cancellation"
        );
        let answered = gate.respond(&request_id, ApprovalDecision::Allow).await;
        assert!(
            !answered,
            "the decisions entry must be cleaned up too — a late response finds nothing"
        );
    }

    #[tokio::test]
    async fn timed_out_approval_cleans_up_pending_entries() {
        // A missing responder must deny after the bounded answer timeout and
        // remove every pending entry, not leak the request.
        let broker = ApprovalBroker::new();
        let gate = Arc::new(
            InteractiveApprovalGate::new(broker.clone())
                .with_answer_timeout(Duration::from_millis(100)),
        );
        let gate_for_task = gate.clone();

        let handle = tokio::spawn(async move {
            gate_for_task
                .authorize(&write_call(), &write_spec(), &CancellationToken::new())
                .await
        });

        let request_id = wait_for_request(&broker).await;
        let result = handle.await.unwrap();
        assert!(
            result.is_err(),
            "a timed-out approval must deny: {result:?}"
        );
        assert!(
            broker.pending().await.is_empty(),
            "no pending request may remain after a timeout"
        );
        let answered = gate.respond(&request_id, ApprovalDecision::Allow).await;
        assert!(
            !answered,
            "the decisions entry must be cleaned up after a timeout"
        );
    }

    #[tokio::test]
    async fn answered_approval_resolves_and_cleans_up() {
        // The happy path still resolves and removes the pending request.
        let broker = ApprovalBroker::new();
        let gate = Arc::new(InteractiveApprovalGate::new(broker.clone()));
        let gate_for_task = gate.clone();

        let handle = tokio::spawn(async move {
            gate_for_task
                .authorize(&write_call(), &write_spec(), &CancellationToken::new())
                .await
        });

        let request_id = wait_for_request(&broker).await;
        let answered = gate.respond(&request_id, ApprovalDecision::Allow).await;
        assert!(answered, "the responder must find the pending request");
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, ApprovalDecision::Allow);
        assert!(
            broker.pending().await.is_empty(),
            "an answered request must leave the broker"
        );
    }

    /// Records every delegated call so tests can prove the standing layer
    /// did (or did not) fall through to the underlying gate.
    struct RecordingGate {
        calls: Mutex<Vec<(String, ToolRisk)>>,
        decision: ApprovalDecision,
    }

    impl RecordingGate {
        fn denying() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                decision: ApprovalDecision::Deny,
            })
        }

        fn allowing() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                decision: ApprovalDecision::Allow,
            })
        }
    }

    #[async_trait::async_trait]
    impl ApprovalGate for RecordingGate {
        async fn authorize(
            &self,
            call: &ToolCall,
            spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            self.calls.lock().await.push((call.name.clone(), spec.risk));
            Ok(self.decision)
        }
    }

    fn write_call_at(path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "fs.write".into(),
            arguments: json!({"path": path, "content": content}),
        }
    }

    fn process_call(command: &str) -> ToolCall {
        ToolCall {
            id: "c2".into(),
            name: "shell.exec".into(),
            arguments: json!({"command": command}),
        }
    }

    fn process_spec() -> ToolSpec {
        ToolSpec {
            name: "shell.exec".into(),
            description: "run a command".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    fn process_run_spec() -> ToolSpec {
        ToolSpec {
            name: "process.run".into(),
            description: "run argv".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    fn process_run_call(argv: &[&str]) -> ToolCall {
        ToolCall {
            id: "c2".into(),
            name: "process.run".into(),
            arguments: json!({"argv": argv}),
        }
    }

    fn process_session_spec() -> ToolSpec {
        ToolSpec {
            name: "process.session".into(),
            description: "session".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }
    }

    fn process_session_call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c2".into(),
            name: "process.session".into(),
            arguments,
        }
    }

    fn exec_grant(id: &str, argv: &[&str], max_runs: Option<u32>) -> StandingGrant {
        StandingGrant {
            id: id.into(),
            risk: ToolRisk::ProcessExecution,
            target: GrantTarget {
                exec_argv_prefix: Some(argv.iter().map(|token| (*token).to_string()).collect()),
                ..Default::default()
            },
            constraint: agent_contracts::GrantConstraint {
                max_runs,
                ..Default::default()
            },
            expires_at_ms: u64::MAX,
        }
    }

    fn shell_grant(id: &str, command: &str, max_runs: Option<u32>) -> StandingGrant {
        StandingGrant {
            id: id.into(),
            risk: ToolRisk::ProcessExecution,
            target: GrantTarget {
                shell_command_digest: Some(agent_contracts::shell_command_digest(command)),
                ..Default::default()
            },
            constraint: agent_contracts::GrantConstraint {
                max_runs,
                ..Default::default()
            },
            expires_at_ms: u64::MAX,
        }
    }

    fn write_grant(id: &str, prefix: &str, expires_at_ms: u64) -> StandingGrant {
        StandingGrant {
            id: id.into(),
            risk: ToolRisk::WorkspaceWrite,
            target: GrantTarget {
                workspace_path_prefix: Some(prefix.into()),
                ..Default::default()
            },
            constraint: Default::default(),
            expires_at_ms,
        }
    }

    #[tokio::test]
    async fn standing_grant_allows_matching_write_without_prompt() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();

        let decision = gate
            .authorize(
                &write_call_at("src/main.rs", "fn main() {}"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Allow,
            "a write inside the grant must be allowed without prompting"
        );
        assert!(
            inner.calls.lock().await.is_empty(),
            "the underlying gate must not be consulted for a granted call"
        );
        let audit = gate.recent_decisions().await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].grant_id.as_deref(), Some("g-src"));
    }

    #[tokio::test]
    async fn write_outside_grant_delegates_to_inner() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();

        let decision = gate
            .authorize(
                &write_call_at("lib/other.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "an ungranted write must fall through to the underlying gate"
        );
        assert_eq!(
            inner.calls.lock().await.len(),
            1,
            "the underlying gate decides ungranted calls"
        );
    }

    #[tokio::test]
    async fn grant_prefix_is_component_aware() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-src", "src", u64::MAX))
            .await
            .unwrap();

        // `src/main.rs` is inside `src`; `src-other/x` is a different
        // component and must not match. (A file literally named `src` is
        // the prefix itself, which is covered — component equality.)
        assert_eq!(
            gate.authorize(
                &write_call_at("src/main.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
            ApprovalDecision::Allow
        );
        let decision = gate
            .authorize(
                &write_call_at("src-other/x", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "prefix matching must be component-aware"
        );
        assert_eq!(inner.calls.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn parent_and_absolute_writes_never_match_a_grant() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g", "src/", u64::MAX))
            .await
            .unwrap();

        for hostile in [
            "src/../outside/x",
            "../src/x",
            "C:\\outside\\x",
            "/etc/passwd",
        ] {
            let decision = gate
                .authorize(
                    &write_call_at(hostile, "x"),
                    &write_spec(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(
                decision,
                ApprovalDecision::Deny,
                "an escaping write must never be granted: {hostile}"
            );
        }
        assert_eq!(
            inner.calls.lock().await.len(),
            4,
            "every escaping write must be delegated (and denied by the inner gate)"
        );
    }

    #[tokio::test]
    async fn expired_grant_stops_matching() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let inner = RecordingGate::denying();
        let clock = Arc::new(AtomicU64::new(0));
        let clock_for_gate = clock.clone();
        let gate = TaskApprovalGate::with_clock(
            inner.clone(),
            Box::new(move || clock_for_gate.load(Ordering::Relaxed)),
        );
        gate.grant(write_grant("g-exp", "src/", 1_000))
            .await
            .unwrap();

        // While the clock is before the expiry the grant covers the write...
        let decision = gate
            .authorize(
                &write_call_at("src/main.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);

        // ...past the expiry it stops matching and the call falls through.
        clock.store(2_000, Ordering::Relaxed);
        let decision = gate
            .authorize(
                &write_call_at("src/main.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny);
        assert_eq!(inner.calls.lock().await.len(), 1);
        assert!(
            gate.active_grants().await.is_empty(),
            "expired grants must not be reported as active"
        );
    }

    #[tokio::test]
    async fn revoked_grant_stops_matching() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-r", "src/", u64::MAX))
            .await
            .unwrap();
        assert!(gate.revoke("g-r").await, "the grant must be live");
        assert!(!gate.revoke("g-r").await, "a second revoke finds nothing");

        let decision = gate
            .authorize(
                &write_call_at("src/main.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "a revoked grant must not cover writes anymore"
        );
    }

    #[tokio::test]
    async fn process_run_grant_is_structured_argv_prefix() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(exec_grant("g-test", &["cargo", "test"], Some(2)))
            .await
            .unwrap();

        for argv in [
            &["cargo", "test"][..],
            &["cargo", "test", "--", "--nocapture"][..],
        ] {
            let decision = gate
                .authorize(
                    &process_run_call(argv),
                    &process_run_spec(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(
                decision,
                ApprovalDecision::Allow,
                "argv prefix must cover extra args: {argv:?}"
            );
        }
        let decision = gate
            .authorize(
                &process_run_call(&["cargo", "test"]),
                &process_run_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny, "the run cap must bite");

        let decision = gate
            .authorize(
                &process_run_call(&["cargo", "testx"]),
                &process_run_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "argv matching must keep argument boundaries"
        );
        assert_eq!(inner.calls.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shell_grant_is_exact_digest_and_rejects_conjunction() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner);
        gate.grant(shell_grant("g-shell", "git status", Some(2)))
            .await
            .unwrap();
        assert_eq!(
            gate.authorize(
                &process_call("git status"),
                &process_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
            ApprovalDecision::Allow
        );
        assert_eq!(
            gate.authorize(
                &process_call("git status && something-dangerous"),
                &process_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
            ApprovalDecision::Deny,
            "shell && must not inherit an exact-command grant"
        );
        assert_eq!(
            gate.authorize(
                &process_call("git status --porcelain"),
                &process_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
            ApprovalDecision::Deny,
            "shell grants are not command prefixes"
        );
    }

    #[tokio::test]
    async fn content_cap_rejects_oversized_write() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(StandingGrant {
            id: "g-small".into(),
            risk: ToolRisk::WorkspaceWrite,
            target: GrantTarget {
                workspace_path_prefix: Some("src/".into()),
                ..Default::default()
            },
            constraint: agent_contracts::GrantConstraint {
                max_content_bytes: Some(10),
                max_runs: None,
            },
            expires_at_ms: u64::MAX,
        })
        .await
        .unwrap();

        let decision = gate
            .authorize(
                &write_call_at("src/small.rs", "tiny"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
        let decision = gate
            .authorize(
                &write_call_at("src/big.rs", "this content is far too large"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "the content cap is part of the effect scope"
        );
    }

    fn patch_spec() -> ToolSpec {
        ToolSpec {
            name: "edit.patch".into(),
            description: "patch".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }
    }

    fn multi_file_patch_call(entries: &[(&str, &str, &str)]) -> ToolCall {
        let files: Vec<_> = entries
            .iter()
            .map(|(path, old, new)| json!({"path": path, "hunks": [{"old": old, "new": new}]}))
            .collect();
        ToolCall {
            id: "c".into(),
            name: "edit.patch".into(),
            arguments: json!({"files": files}),
        }
    }

    #[tokio::test]
    async fn multi_file_patch_grant_covers_every_target_not_just_the_first() {
        // MOD-AUTH-01 regression: `edit.patch files[]` used to derive a
        // single-path intent from the first file, so a `src/` standing
        // grant authorized writes to every other file in the set. The
        // intent now carries the whole target set and the grant must
        // cover each path.
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();

        let widened = multi_file_patch_call(&[("src/a.rs", "a", "aa"), ("secret/b.rs", "b", "bb")]);
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&widened, &patch_spec()),
            EffectIntent::WorkspaceWriteSet {
                writes: vec![
                    agent_contracts::WorkspaceWriteBound {
                        path: "src/a.rs".into(),
                        max_bytes: 2,
                    },
                    agent_contracts::WorkspaceWriteBound {
                        path: "secret/b.rs".into(),
                        max_bytes: 2,
                    },
                ],
            },
            "multi-file edit.patch must carry every target in its intent"
        );
        assert_eq!(
            gate.authorize(&widened, &patch_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Deny,
            "a src/ grant must not be widened to secret/ by the second file"
        );

        let all_inside = multi_file_patch_call(&[("src/a.rs", "a", "aa"), ("src/b.rs", "b", "bb")]);
        assert_eq!(
            gate.authorize(&all_inside, &patch_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Allow,
            "every target inside the prefix stays granted"
        );

        // One `files[]` entry keeps the single-resource intent shape, so
        // grants minted from single-file calls still match exactly.
        let single = multi_file_patch_call(&[("src/a.rs", "a", "aa")]);
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&single, &patch_spec()),
            EffectIntent::WorkspaceWrite {
                path: "src/a.rs".into(),
                content_bytes: 2,
            }
        );
    }

    #[tokio::test]
    async fn multi_file_patch_content_cap_counts_the_whole_set() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(StandingGrant {
            id: "g-small".into(),
            risk: ToolRisk::WorkspaceWrite,
            target: GrantTarget {
                workspace_path_prefix: Some("src/".into()),
                ..Default::default()
            },
            constraint: agent_contracts::GrantConstraint {
                max_content_bytes: Some(3),
                max_runs: None,
            },
            expires_at_ms: u64::MAX,
        })
        .await
        .unwrap();

        let oversized =
            multi_file_patch_call(&[("src/a.rs", "a", "aaaa"), ("src/b.rs", "b", "bb")]);
        assert_eq!(
            gate.authorize(&oversized, &patch_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Deny,
            "the byte cap sums the whole set, not the first file"
        );
    }

    #[tokio::test]
    async fn grant_rejects_invalid_targets_and_shapes() {
        let gate = TaskApprovalGate::new(RecordingGate::denying());
        let now = now_ms();
        for bad in [
            write_grant("g1", "../escape", now + 10_000),
            write_grant("g2", "/abs", now + 10_000),
        ] {
            assert!(
                gate.grant(bad).await.is_err(),
                "an escaping or absolute grant target must be rejected"
            );
        }
        // A drive-qualified path is absolute on Windows; on Unix `C:\abs`
        // is an unusual but legal relative name, so assert the Windows
        // absolute form only where it actually is one.
        #[cfg(windows)]
        assert!(
            gate.grant(write_grant("g3", "C:\\abs", now + 10_000))
                .await
                .is_err(),
            "a Windows drive path must be rejected as an absolute target"
        );
        // A read-only grant is not an effect grant.
        let read_only = StandingGrant {
            id: "g4".into(),
            risk: ToolRisk::ReadOnly,
            target: GrantTarget {
                workspace_path_prefix: Some("src/".into()),
                ..Default::default()
            },
            constraint: Default::default(),
            expires_at_ms: u64::MAX,
        };
        assert!(gate.grant(read_only).await.is_err());
        // A process grant without a command scope matches nothing.
        let no_scope = StandingGrant {
            id: "g5".into(),
            risk: ToolRisk::ProcessExecution,
            target: GrantTarget::default(),
            constraint: Default::default(),
            expires_at_ms: u64::MAX,
        };
        assert!(gate.grant(no_scope).await.is_err());
        // An already-expired grant cannot be established.
        assert!(
            gate.grant(write_grant("g6", "src/", now - 1))
                .await
                .is_err()
        );
        assert!(
            gate.active_grants().await.is_empty(),
            "no invalid grant may be live"
        );
    }

    #[tokio::test]
    async fn zero_responder_without_grant_denies_without_expansion() {
        // The acceptance criterion: with no user responses, an ungranted
        // call is denied (quickly, by a short interactive timeout) — never
        // implicitly allowed, and a granted call never waits at all.
        let broker = ApprovalBroker::new();
        let interactive = Arc::new(
            InteractiveApprovalGate::new(broker.clone())
                .with_answer_timeout(Duration::from_millis(50)),
        );
        let gate = TaskApprovalGate::new(interactive.clone());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();

        // Granted: no prompt, no wait.
        let start = tokio::time::Instant::now();
        let decision = gate
            .authorize(
                &write_call_at("src/main.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
        assert!(
            start.elapsed() < Duration::from_millis(40),
            "a granted call must not wait for a responder"
        );

        // Ungranted: denied when no responder appears — no privilege
        // expansion, no five-minute stall. The interactive inner reports a
        // timeout as `Err(ApprovalDenied)` (a deny, never an allow).
        let result = gate
            .authorize(
                &write_call_at("elsewhere/secret.rs", "x"),
                &write_spec(),
                &CancellationToken::new(),
            )
            .await;
        match result {
            Ok(ApprovalDecision::Deny) => {}
            Err(AgentError::ApprovalDenied(_)) => {}
            other => panic!("no responder must deny, got {other:?}"),
        }
    }

    #[test]
    fn derive_effect_intent_extracts_the_concrete_effect_from_arguments() {
        let write = ToolCall {
            id: "c".into(),
            name: "fs.write".into(),
            arguments: json!({"path": "src/main.rs", "content": "fn main() {}"}),
        };
        let write_spec = ToolSpec {
            name: "fs.write".into(),
            description: "w".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        };
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&write, &write_spec),
            EffectIntent::WorkspaceWrite {
                path: "src/main.rs".into(),
                content_bytes: "fn main() {}".len() as u64,
            }
        );

        // `edit.replace` declares content under `new`; the intent must use it.
        let edit = ToolCall {
            id: "c".into(),
            name: "edit.replace".into(),
            arguments: json!({"path": "src/main.rs", "old": "a", "new": "b"}),
        };
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&edit, &write_spec),
            EffectIntent::WorkspaceWrite {
                path: "src/main.rs".into(),
                content_bytes: 1,
            }
        );

        let process = ToolCall {
            id: "c".into(),
            name: "shell.exec".into(),
            arguments: json!({"command": "cargo test -- --nocapture"}),
        };
        let process_spec = ToolSpec {
            name: "shell.exec".into(),
            description: "p".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        };
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&process, &process_spec),
            agent_contracts::shell_exec_intent("", "cargo test -- --nocapture")
        );

        let read = ToolCall {
            id: "c".into(),
            name: "fs.read".into(),
            arguments: json!({"path": "src/main.rs"}),
        };
        let read_spec = ToolSpec {
            name: "fs.read".into(),
            description: "r".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        };
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&read, &read_spec),
            EffectIntent::ReadOnly
        );
    }

    #[tokio::test]
    async fn process_run_grant_matches_argv_not_an_unused_command_field() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner);
        gate.grant(exec_grant("g-cargo", &["cargo"], None))
            .await
            .unwrap();

        let decision = gate
            .authorize(
                &process_run_call(&["cargo", "test"]),
                &process_run_spec(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Allow,
            "process.run argv must be the grant bound"
        );

        let widened = ToolCall {
            id: "c2".into(),
            name: "process.run".into(),
            arguments: json!({"command": "cargo test", "argv": ["rm", "-rf", "."]}),
        };
        let decision = gate
            .authorize(&widened, &process_run_spec(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "an unused command field must not cover a different argv"
        );
    }

    #[tokio::test]
    async fn process_session_poll_does_not_consume_a_process_grant() {
        let inner = RecordingGate::denying();
        let gate = TaskApprovalGate::new(inner);
        gate.grant(exec_grant("g-cargo", &["cargo"], Some(1)))
            .await
            .unwrap();

        let start = process_session_call(json!({
            "action": "start",
            "argv": ["cargo", "test"]
        }));
        assert_eq!(
            gate.authorize(&start, &process_session_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Allow
        );

        let poll = process_session_call(json!({
            "action": "poll",
            "session_id": "s1",
            "argv": ["cargo", "test"]
        }));
        assert_eq!(
            gate.authorize(&poll, &process_session_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Allow,
            "poll must not need a second command-prefix grant"
        );
        let shadow = gate.shadow_verdict(&poll, &process_session_spec()).await;
        assert!(
            matches!(shadow, ShadowVerdict::Granted { .. }),
            "shadow must not be stricter than legacy Allow for poll: {shadow:?}"
        );

        let stop = process_session_call(json!({
            "action": "stop",
            "session_id": "s1",
            "argv": ["cargo", "test"]
        }));
        assert_eq!(
            gate.authorize(&stop, &process_session_spec(), &CancellationToken::new())
                .await
                .unwrap(),
            ApprovalDecision::Allow,
            "stop must not consume a command-prefix grant"
        );

        let second_start = process_session_call(json!({
            "action": "start",
            "argv": ["cargo", "test"]
        }));
        assert_eq!(
            gate.authorize(
                &second_start,
                &process_session_spec(),
                &CancellationToken::new()
            )
            .await
            .unwrap(),
            ApprovalDecision::Deny,
            "poll must not have refunded the start's run cap"
        );
    }

    #[test]
    fn derive_effect_intent_is_fail_closed_on_missing_arguments() {
        // A write without a path yields an empty-path intent, which can
        // never match a grant — the legacy parser behaved identically
        // (missing path => no match).
        let missing = ToolCall {
            id: "c".into(),
            name: "fs.write".into(),
            arguments: json!({"content": "x"}),
        };
        let write_spec = ToolSpec {
            name: "fs.write".into(),
            description: "w".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        };
        assert_eq!(
            TaskApprovalGate::derive_effect_intent(&missing, &write_spec),
            EffectIntent::WorkspaceWrite {
                path: String::new(),
                content_bytes: 1,
            }
        );

        // An empty-path intent never matches any path prefix.
        let grant = StandingGrant {
            id: "g".into(),
            risk: ToolRisk::WorkspaceWrite,
            target: GrantTarget {
                workspace_path_prefix: Some("src".into()),
                ..Default::default()
            },
            constraint: Default::default(),
            expires_at_ms: u64::MAX,
        };
        let intent = TaskApprovalGate::derive_effect_intent(&missing, &write_spec);
        assert!(!TaskApprovalGate::grant_matches(&grant, &intent));
    }

    // --- IntentShadowGate (ACI v2 shadow mode) ---

    #[tokio::test]
    async fn shadow_verdict_grants_read_only_without_a_grant() {
        let gate = TaskApprovalGate::new(RecordingGate::denying());
        let read_only = ToolSpec {
            name: "fs.read".into(),
            description: "read".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::ReadResource],
        };
        let verdict = gate
            .shadow_verdict(
                &ToolCall {
                    id: "c".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/main.rs"}),
                },
                &read_only,
            )
            .await;
        assert!(
            matches!(verdict, ShadowVerdict::Granted { ref grant_id, .. } if grant_id == "read-only"),
            "read-only needs no grant: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn shadow_verdict_grants_when_a_grant_matches_and_denies_otherwise() {
        let gate = TaskApprovalGate::new(RecordingGate::denying());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();

        let granted = gate
            .shadow_verdict(&write_call_at("src/main.rs", "x"), &write_spec())
            .await;
        assert!(
            matches!(granted, ShadowVerdict::Granted { ref grant_id, .. } if grant_id == "g-src"),
            "a matching grant must grant in shadow: {granted:?}"
        );

        let denied = gate
            .shadow_verdict(&write_call_at("elsewhere/x.rs", "x"), &write_spec())
            .await;
        assert!(
            matches!(denied, ShadowVerdict::Denied { ref reason } if reason.contains("no live standing grant")),
            "an ungranted write must be denied by the v2 policy: {denied:?}"
        );
    }

    #[tokio::test]
    async fn shadow_verdict_respects_expiry_and_run_caps_like_the_legacy_path() {
        // Expired grant: the legacy path would delegate (and a missing
        // responder denies); shadow must not grant either. `grant()` refuses
        // an already-expired grant, so the expiry is produced by moving the
        // injected clock past the grant's deadline.
        let now = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let now_for_gate = now.clone();
        let expired = TaskApprovalGate::with_clock(
            RecordingGate::denying(),
            Box::new(move || now_for_gate.load(std::sync::atomic::Ordering::SeqCst)),
        );
        expired
            .grant(write_grant("g-old", "src/", 2_000))
            .await
            .unwrap();
        now.store(3_000, std::sync::atomic::Ordering::SeqCst);
        let verdict = expired
            .shadow_verdict(&write_call_at("src/main.rs", "x"), &write_spec())
            .await;
        assert!(
            matches!(verdict, ShadowVerdict::Denied { .. }),
            "an expired grant must not grant: {verdict:?}"
        );

        // Run-capped process grant: after the cap is consumed on the legacy
        // path, shadow must not grant — otherwise shadow would look wider
        // than the legacy gate.
        let process = TaskApprovalGate::new(RecordingGate::denying());
        process
            .grant(shell_grant("g-run", "cargo test", Some(2)))
            .await
            .unwrap();
        for _ in 0..2 {
            let decision = process
                .authorize(
                    &process_call("cargo test"),
                    &process_spec(),
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(decision, ApprovalDecision::Allow);
        }
        let verdict = process
            .shadow_verdict(&process_call("cargo test"), &process_spec())
            .await;
        assert!(
            matches!(verdict, ShadowVerdict::Denied { .. }),
            "an exhausted run cap must not grant in shadow: {verdict:?}"
        );
    }

    #[tokio::test]
    async fn shadow_verdict_never_grants_beyond_the_legacy_path() {
        // The hard invariant of shadow mode: whenever shadow says Granted,
        // the legacy path must say Allow. Exercise a mix of granted and
        // ungranted writes/process calls against a permissive inner gate.
        let inner = RecordingGate::allowing();
        let gate = TaskApprovalGate::new(inner.clone());
        gate.grant(write_grant("g-src", "src/", u64::MAX))
            .await
            .unwrap();
        gate.grant(exec_grant("g-run", &["cargo"], None))
            .await
            .unwrap();
        gate.grant(shell_grant("g-shell", "cargo test", None))
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let cases: Vec<(ToolCall, ToolSpec)> = vec![
            (write_call_at("src/main.rs", "x"), write_spec()),
            (write_call_at("elsewhere/x.rs", "x"), write_spec()),
            (process_call("cargo test"), process_spec()),
            (process_call("npm install"), process_spec()),
            (
                process_session_call(json!({
                    "action": "poll",
                    "session_id": "s1",
                    "argv": ["cargo", "test"]
                })),
                process_session_spec(),
            ),
        ];
        for (call, spec) in cases {
            let shadow = gate.shadow_verdict(&call, &spec).await;
            let legacy = gate.authorize(&call, &spec, &cancel).await.unwrap();
            if matches!(shadow, ShadowVerdict::Granted { .. }) {
                assert_eq!(
                    legacy,
                    ApprovalDecision::Allow,
                    "shadow Granted for '{}' must imply legacy Allow",
                    call.name
                );
            }
        }
    }
}

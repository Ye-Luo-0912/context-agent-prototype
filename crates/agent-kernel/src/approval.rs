use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ApprovalGate, CancellationToken, ToolCall, ToolRisk,
    ToolSpec,
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ToolRisk, ToolSpec};
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
}

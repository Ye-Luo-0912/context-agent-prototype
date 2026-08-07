use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ApprovalDecision, ApprovalGate, ToolCall, ToolRisk, ToolSpec,
};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::time::timeout;
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
    async fn authorize(&self, _call: &ToolCall, spec: &ToolSpec) -> AgentResult<ApprovalDecision> {
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
    async fn authorize(&self, call: &ToolCall, spec: &ToolSpec) -> AgentResult<ApprovalDecision> {
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

        match timeout(self.answer_timeout, rx).await {
            Ok(Ok(decision)) => Ok(decision),
            Ok(Err(_)) => {
                self.broker.remove(&request_id).await;
                Err(AgentError::ApprovalDenied(
                    "approval request dropped (no responder)".into(),
                ))
            }
            Err(_) => {
                self.broker.remove(&request_id).await;
                Err(AgentError::ApprovalDenied(
                    "approval request timed out (no response)".into(),
                ))
            }
        }
    }
}

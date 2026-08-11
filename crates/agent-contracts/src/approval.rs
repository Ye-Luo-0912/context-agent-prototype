use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, CancellationToken, ToolCall, ToolRisk, ToolSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Target scope of a standing grant: what the grant covers. At least one of
/// the scopes must be set (a grant with neither matches nothing and is
/// rejected at grant time).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantTarget {
    /// Workspace-relative path prefix (component-aware): a workspace write
    /// whose path is at or under this prefix is covered. `None` = no path
    /// scope.
    pub workspace_path_prefix: Option<String>,
    /// Lexical command prefix (whitespace-separated tokens): a process call
    /// whose command starts with these tokens is covered. `None` = no
    /// command scope.
    pub process_command_prefix: Option<String>,
}

/// Bounded resource envelope of a standing grant. A `None` limit means the
/// grant does not constrain that dimension.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantConstraint {
    /// Byte cap on the content of a workspace write covered by the grant.
    pub max_content_bytes: Option<u64>,
    /// Run cap on process executions covered by the grant (consumed once
    /// per matched call).
    pub max_runs: Option<u32>,
}

/// A trusted, task-scoped standing grant: a narrow effect (`risk`) with a
/// target scope, a bounded resource constraint and an expiry. The model can
/// *use* a matching grant (the call is allowed without a per-call prompt)
/// but can never create, widen or extend one — grants are established by
/// the composition root / UI and only shrink (revocation, consumption,
/// expiry). An expired, revoked or exceeded grant silently stops matching,
/// and the call falls through to the underlying gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingGrant {
    pub id: String,
    pub risk: ToolRisk,
    pub target: GrantTarget,
    pub constraint: GrantConstraint,
    /// Expiry as epoch milliseconds; a grant at or past this instant is
    /// inert.
    pub expires_at_ms: u64,
}

/// Decides whether one tool call may run. The `cancel` token lets a
/// waiting gate (interactive prompt, standing-grant negotiation) abort when
/// the operation itself is cancelled — a cancelled turn must not leave a
/// pending approval request behind, and a gate that waits (up to a bounded
/// answer timeout) must stop waiting the moment its caller is gone.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn authorize(
        &self,
        call: &ToolCall,
        spec: &ToolSpec,
        cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision>;
}

/// The v2 perspective on one approval decision, computed *beside* the
/// legacy gate (shadow mode): what an intent-based `AuthorityGate` would
/// decide. The v2 policy is deny-by-default — only a live standing grant
/// whose target scope contains the derived effect intent allows the call —
/// so `Denied` is the normal answer for an ungranted write/process call,
/// and read-only calls need no grant at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowVerdict {
    /// The derived intent falls inside a live standing grant (or is
    /// read-only and needs no grant).
    Granted { grant_id: String, reason: String },
    /// The derived intent matches no live grant; a v2 gate would refuse.
    Denied { reason: String },
}

/// The shadow authority seam (ACI v2 compatibility order step 4): an
/// intent-derived decision recorded beside the legacy `ApprovalGate`
/// without being enforced. The kernel runs both paths when a shadow gate is
/// configured and publishes the comparison, so the invariant trace —
/// granted/denied/reason — can be checked against the legacy path before
/// the v2 gate is ever enforced. The one hard invariant is that the shadow
/// gate never *grants* beyond the legacy gate: `Granted` in shadow must
/// imply `Allow` on the legacy path, otherwise the v2 policy has a
/// privilege-expansion bug.
#[async_trait]
pub trait IntentShadowGate: Send + Sync {
    async fn shadow_verdict(&self, call: &ToolCall, spec: &ToolSpec) -> ShadowVerdict;
}

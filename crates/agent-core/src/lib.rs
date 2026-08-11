mod approval;
mod authority;
mod capability_admission;
mod kernel;

pub use approval::{
    ApprovalBroker, ApprovalRequest, GrantAuditEntry, InteractiveApprovalGate, PolicyApprovalGate,
    TaskApprovalGate,
};
pub use authority::{
    ApprovalAuthority, ApprovalVerdict, EffectAuthority, EventAuthority, OutputAuthority,
};
pub use capability_admission::{
    AdmissionContext, CapabilityAdmission, MAX_TOOL_DESCRIPTION_CHARS, MAX_TOOL_NAME_CHARS,
    MAX_TOOL_SCHEMA_BYTES, MAX_TOOLS_PER_CAPABILITY,
};
pub use kernel::{CoreAuthority, CoreAuthorityConfig};

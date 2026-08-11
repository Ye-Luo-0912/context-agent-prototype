mod approval;
mod authority;
mod capability_admission;
mod capability_state;
mod kernel;
mod plugin_admission;

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
pub use capability_state::{CapabilityState, CapabilityStateAuthority};
pub use kernel::{CoreAuthority, CoreAuthorityConfig};
pub use plugin_admission::{
    MAX_ADAPTERS_PER_PACKAGE, MAX_COMMAND_ARG_CHARS, MAX_COMMAND_ARGS, MAX_COMPONENT_ID_CHARS,
    MAX_COMPONENT_SUMMARY_CHARS, MAX_DEPENDENCIES_PER_PACKAGE, MAX_ENDPOINT_CHARS, MAX_EVENT_CHARS,
    MAX_HOOKS_PER_PACKAGE, MAX_PACKAGE_NAME_CHARS, MAX_PACKAGE_SUMMARY_CHARS, MAX_PROTOCOL_CHARS,
    MAX_RANGE_CHARS, MAX_REFERENCE_CHARS, MAX_SKILLS_PER_PACKAGE, MAX_TESTS_PER_PACKAGE,
    MAX_TOOLS_PER_PACKAGE, MAX_VERSION_CHARS, PluginPackageAdmission,
};

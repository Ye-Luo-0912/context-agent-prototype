mod approval;
mod authority;
mod broker;
mod capability_admission;
mod capability_state;
mod kernel;
mod operation;
mod plugin_admission;
mod plugin_state;
mod port;

pub use approval::{
    ApprovalBroker, ApprovalRequest, GrantAuditEntry, InteractiveApprovalGate, PolicyApprovalGate,
    TaskApprovalGate,
};
pub use authority::ApprovalVerdict;
pub use broker::{
    CoordinatorReply, CoordinatorRequest, JournaledEffectBroker, ProcessEffectBroker,
    ReservationJournal, ReservedRecord, serve_broker_lines,
};
pub use capability_admission::{
    AdmissionContext, CapabilityAdmission, MAX_TOOL_DESCRIPTION_CHARS, MAX_TOOL_NAME_CHARS,
    MAX_TOOL_SCHEMA_BYTES, MAX_TOOLS_PER_CAPABILITY,
};
pub use capability_state::{CapabilityState, CapabilityStateAuthority};
pub use kernel::CoreAuthorityConfig;
pub use plugin_admission::{
    MAX_ADAPTERS_PER_PACKAGE, MAX_COMMAND_ARG_CHARS, MAX_COMMAND_ARGS, MAX_COMPONENT_ID_CHARS,
    MAX_COMPONENT_SUMMARY_CHARS, MAX_DEPENDENCIES_PER_PACKAGE, MAX_ENDPOINT_CHARS, MAX_EVENT_CHARS,
    MAX_HOOKS_PER_PACKAGE, MAX_PACKAGE_NAME_CHARS, MAX_PACKAGE_SUMMARY_CHARS, MAX_PROTOCOL_CHARS,
    MAX_RANGE_CHARS, MAX_REFERENCE_CHARS, MAX_SKILLS_PER_PACKAGE, MAX_TESTS_PER_PACKAGE,
    MAX_TOOLS_PER_PACKAGE, MAX_VERSION_CHARS, PluginPackageAdmission,
};
pub use plugin_state::PluginStateAuthority;
pub use port::{
    AdmittedToolPermit, CorePort, CoreToolExecution, EffectAck, EffectBroker,
    EffectCommitDisposition, EffectCommitRejection, EffectCommitRequest, EffectReservation,
    EffectRollbackRequest, LocalEffectBroker, OperationCancelDisposition, PublishedToolPermit,
    ReservedEffect, ToolOperationAdmission, build_core_port, try_build_core_port,
};

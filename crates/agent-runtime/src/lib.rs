//! Agent runtime framework: an actor that owns the mutable runtime state
//! (`RuntimeHandle` -> `mpsc<RuntimeCommand>` -> `RuntimeActor`) and a module
//! host that composes typed capabilities with a uniform lifecycle.

mod actor;
pub mod budget;
mod capability;
pub mod checkpoint;
mod command;
mod execution;
// 定向再导出：类型已随 `TaskRecord::resume` 公开，replay/审计需要直接
// 驱动同一投影以重建证据前沿；execution 模块本身保持私有。
pub use execution::{
    ExecutionState, FrontierObservation, NegativeExecutionFact, VerificationSourceLease,
};
pub mod host;
mod instance;
pub mod modules;
mod output;
mod platform;
mod plugin;
mod policy;
mod prompt;
mod services;
mod sink;
mod surface;
pub mod task;

pub use actor::spawn_runtime;
pub use budget::{
    DEFAULT_OUTPUT_RESERVE, ModelBudget, approx_layer_tokens, engine_pack_window,
    provider_send_window,
};
pub use capability::{
    CapabilityAwareDispatcher, CapabilityCatalogEntry, CapabilityRegistry, CapabilityRunState,
};
pub use checkpoint::{
    CapabilitySnapshot, RUNTIME_CHECKPOINT_VERSION, RunMetadata, RuntimeCheckpoint,
    TaskManagerSnapshot, TaskRecordSnapshot,
};
pub use command::RuntimeHandle;
pub use host::{
    APPROVAL_POLICY, ARTIFACT_STORE, CONTEXT_SERVICE, CapabilityId, EVENT_STORE, MODEL_PROVIDER,
    Module, ModuleHost, ServiceRegistry, TOOL_PROVIDER,
};
pub use instance::RuntimeInstance;
pub use modules::{
    ApprovalModule, ArtifactModule, ContextModule, EventModule, ModelModule, ToolModule,
};
pub use platform::{
    AuthenticatedOperationControlAdapter, BoundSessionAuthorizer,
    MAX_OPERATION_CONTROL_ENVELOPE_BYTES, MAX_OPERATION_CONTROL_SESSIONS,
    OperationAcceptedSubscription, OperationControlAction, OperationControlAuthorization,
    OperationControlAuthorizationRequest, OperationControlAuthorizer, OperationControlGrant,
    OperationControlRouter, OperationControlSessionRegistry,
};
pub use plugin::{
    HookRef, HookView, PLUGIN_TEST_OUTPUT_TAIL_CHARS, PLUGIN_TEST_TIMEOUT, PluginPackageView,
    PluginRegistry, PluginTestReport, PluginTestResult, SkillView,
};
pub use prompt::{PromptAssembler, focus_frame_tokens};
pub use services::{AuthorityRecoveryServices, RuntimeServices};
pub use task::{
    AnchorPatch, ContextRootClaim, RootClaimRole, RootClaimStrength, TaskAnchor, TaskInfo,
    TaskManager, TaskRecord, TaskStatus, TaskToolRequirementSet, anchor_root_claims,
    task_anchor_view,
};

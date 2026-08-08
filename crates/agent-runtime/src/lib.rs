//! Agent runtime framework: an actor that owns the mutable runtime state
//! (`RuntimeHandle` -> `mpsc<RuntimeCommand>` -> `RuntimeActor`) and a module
//! host that composes typed capabilities with a uniform lifecycle.

mod actor;
pub mod budget;
mod capability;
mod command;
pub mod host;
mod instance;
pub mod modules;
mod prompt;
mod sink;
pub mod task;

pub use actor::{RuntimeActor, spawn_runtime};
pub use budget::{DEFAULT_OUTPUT_RESERVE, ModelBudget, approx_layer_tokens};
pub use capability::{CapabilityAwareDispatcher, CapabilityCatalogEntry, CapabilityRegistry};
pub use command::{Reply, RuntimeCommand, RuntimeHandle};
pub use host::{
    APPROVAL_POLICY, ARTIFACT_STORE, CONTEXT_SERVICE, CapabilityId, EVENT_STORE, MODEL_PROVIDER,
    Module, ModuleHost, ServiceRegistry, TOOL_PROVIDER,
};
pub use instance::RuntimeInstance;
pub use modules::{
    ApprovalModule, ArtifactModule, ContextModule, EventModule, ModelModule, ToolModule,
};
pub use prompt::PromptAssembler;
pub use task::{TaskInfo, TaskManager, TaskRecord, TaskStatus};

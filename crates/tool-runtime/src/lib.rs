mod registry;
pub mod tools;

pub use agent_contracts::{
    CAPABILITY_INSPECT, CAPABILITY_LOAD, CAPABILITY_SEARCH, CAPABILITY_UNLOAD, ToolCatalogEntry,
    ToolLifecycle,
};
pub use registry::{BuiltinToolDispatcher, ToolLifecycleConfig};
pub use tools::{ShellDialect, ShellKind};

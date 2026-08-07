//! The dynamic capability platform: the extension interface for capabilities
//! an LLM (or any external actor) can register at runtime.
//!
//! This is the second plane of the runtime's extension model. The first
//! plane — trusted core services — stays typed: `ServiceRegistry` lookups
//! for the high-frequency context/model/approval/event/artifact services.
//! The dynamic plane is a capability: a manifest plus a runtime object that
//! advertises tool schemas and handles calls. The module host turns the
//! advertised schemas into part of the runtime's tool provider, so a
//! registered capability is immediately callable by the model.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{AgentResult, ToolCall, ToolOutput, ToolSpec};

/// When a capability's service is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityLifecycle {
    /// Started when the host starts.
    Eager,
    /// Started on first invocation.
    Lazy,
}

/// Maturity of a capability on the self-improvement ladder. External
/// capabilities always enter as `Experimental` — the LLM cannot declare its
/// own module `Stable`; the ladder is climbed through testing and
/// validation (the replay/evaluation infrastructure is the future
/// evaluator), not by declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Newly published; not yet exercised by real runs.
    #[default]
    Experimental,
    /// Exercised by replay/evaluation scenarios.
    Tested,
    /// Passed validation against the evaluation gate.
    Validated,
    /// Approved for normal use.
    Stable,
    /// Superseded; kept only for migration warnings.
    Deprecated,
}

impl CapabilityStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Experimental => "experimental",
            Self::Tested => "tested",
            Self::Validated => "validated",
            Self::Stable => "stable",
            Self::Deprecated => "deprecated",
        }
    }
}

/// How a capability's service is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapabilityTransport {
    InProcess,
    /// A separate process (reserved: the prototype resolves InProcess only).
    Process {
        program: String,
    },
}

/// The declarative part of a dynamic capability: stable identity, a human
/// summary, declared permissions and dependencies, and its lifecycle and
/// transport shape. The host validates dependencies at registration and
/// treats permissions as declarations — the tools the capability advertises
/// carry `ToolRisk` levels that the approval gate enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub summary: String,
    /// Maturity on the Experimental -> Stable ladder. External registrations
    /// are pinned to Experimental by the registry, whatever is declared here.
    #[serde(default)]
    pub status: CapabilityStatus,
    /// Declared permissions, e.g. "workspace:read", "process:run".
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Capability ids this one depends on; the host rejects registration
    /// when a dependency is missing.
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub lifecycle: CapabilityLifecycle,
    pub transport: CapabilityTransport,
}

/// The runtime object behind a capability: a manifest, the tool schemas it
/// exposes to the model, and the invocation handler. The chain is
/// `Service -> Capability -> Tool Schema -> LLM`: a registered capability's
/// tools join the runtime's tool provider, and model calls route back to
/// `invoke`.
#[async_trait]
pub trait Capability: Send + Sync {
    fn manifest(&self) -> &CapabilityManifest;

    /// The tool schemas this capability contributes to the model.
    fn tool_specs(&self) -> Vec<ToolSpec>;

    /// Handle one tool call routed to this capability.
    async fn invoke(&self, call: ToolCall) -> AgentResult<ToolOutput>;

    async fn start(&self) -> AgentResult<()> {
        Ok(())
    }

    async fn stop(&self) -> AgentResult<()> {
        Ok(())
    }
}

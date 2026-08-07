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

/// What a capability provides to the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// A callable tool the model can invoke.
    Tool,
    /// A multi-step, parameterized procedure built from tools.
    Skill,
    /// A background/typed service the runtime talks to.
    Service,
}

/// How a capability's service is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTransport {
    /// In-process trusted core. The first version keeps the trusted core
    /// in-process and never loads Rust plugins: the ABI is not a stable
    /// plugin boundary, and a crashed plugin must not take the runtime down.
    #[serde(alias = "InProcess", alias = "in_process")]
    Builtin,
    /// A separate process speaking a framed protocol (the context-service
    /// shape: versioned handshake, per-request deadlines, frame bounds).
    /// The preferred plane for LLM/third-party extensions, with sandboxing
    /// as a later concern.
    #[serde(alias = "Process")]
    Process { program: String },
    // Future: Wasm — sandboxed in-process plugins. Deliberately not part of
    // the first version.
}

/// The declarative part of a dynamic capability: stable identity, a human
/// summary, what it provides and requires, declared permissions, and its
/// lifecycle and transport shape. The host validates `requires` at
/// registration and treats permissions as declarations — the tools the
/// capability advertises carry `ToolRisk` levels that the approval gate
/// enforces.
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
    /// What this capability provides to the platform.
    #[serde(default)]
    pub provides: Vec<CapabilityKind>,
    /// Declared permissions, e.g. "workspace:read", "process:run".
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Capability ids this one requires; the host rejects registration when
    /// a requirement is missing. The old field name `dependencies` still
    /// deserializes for compatibility.
    #[serde(default, alias = "dependencies")]
    pub requires: Vec<String>,
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

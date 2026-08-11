//! The versioned plugin package manifest: one installable unit that
//! declares its components — tools, skills, hooks, adapters — plus
//! dependencies, permissions, schemas, tests and a compatibility range.
//!
//! Per the ECO-01 decision, skills and hooks are *declared metadata*: they
//! are versioned, validated and source-attributed at install, but the
//! runtime never interprets them (no instruction injection, no lifecycle
//! firing) and they carry no authority of their own. Adapters are metadata
//! too until the adapter plane (ECO-05) interprets them; installing one
//! must not inject its schema catalog into every model request. Only
//! `tools` are interpreted, by the existing capability machinery. The
//! manifest itself is the installation unit: installing it never implies
//! activation or permission (ECO-04).

use serde::{Deserialize, Serialize};

use crate::ToolSpec;

/// A version range for package compatibility, e.g. `"0.1"` or
/// `">=0.2 <1"`. Lexically validated at admission; actual resolution is
/// the installer's job (ECO-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange(pub String);

impl VersionRange {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The mode of a declared hook: observation only, or a gate that can block
/// the lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    /// Observes the lifecycle event without blocking it. Failure policy is
    /// always `HookFailurePolicy::RecordAndContinue` (ECO-07).
    Observe,
    /// May gate (allow/deny) the lifecycle event. Failure policy is always
    /// `HookFailurePolicy::DenyOnFailure` — a gate fails closed (ECO-07).
    Gate,
}

/// What the runtime does when a hook firing itself fails (errors, exceeds
/// its timeout or output bound). Pinned at admission so a gate can never
/// silently fail open (ECO-07): `RecordAndContinue` is only valid for
/// observers, `DenyOnFailure` only for gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookFailurePolicy {
    /// Record the failure (diagnostics) and let the lifecycle event
    /// proceed. Only valid with `HookMode::Observe`.
    #[default]
    RecordAndContinue,
    /// Fail closed: deny/block the lifecycle event. Only valid with
    /// `HookMode::Gate`.
    DenyOnFailure,
}

/// The known lifecycle-event vocabulary a hook may target (ECO-07). This
/// mirrors the runtime maintenance triggers; admission refuses any event
/// outside it, so a misspelled or invented event cannot silently register
/// a hook that never fires.
pub const KNOWN_HOOK_EVENTS: &[&str] = &[
    "user_input",
    "before_model",
    "after_model",
    "after_tool",
    "focus_changed",
    "task_completed",
    "checkpoint",
];

/// A declared hook: a lifecycle observation/gating point. Metadata only
/// (ECO-01): never fired in v0. ECO-07 pins the firing contract in the
/// declaration itself — deterministic ordering, bounded time/output,
/// explicit fail-closed failure policy and permissions that can never
/// widen the package's set — so a future first-class hook runtime has the
/// shape validated at install and needs no new authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDeclaration {
    pub id: String,
    /// The lifecycle event this hook targets, from `KNOWN_HOOK_EVENTS`
    /// (validated at admission).
    pub event: String,
    pub mode: HookMode,
    /// Explicit priority among hooks on the same event: ascending `order`
    /// runs first; ties break by declaration order within a package, then
    /// by package install order (deterministic, ECO-07).
    #[serde(default)]
    pub order: u32,
    /// Wall-clock budget for one firing, capped by
    /// `MAX_HOOK_TIMEOUT_MS` at admission. `None` = runtime default.
    /// Metadata in v0; enforced when hooks become first-class (Gate 5).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Bounded model-facing output for one firing, capped by
    /// `MAX_HOOK_OUTPUT_CHARS` at admission. `None` = runtime default.
    #[serde(default)]
    pub output_budget_chars: Option<usize>,
    /// Failure policy, validated against `mode` at admission: observers
    /// record-and-continue, gates fail closed.
    #[serde(default)]
    pub failure: HookFailurePolicy,
    /// Permissions this hook may exercise. Must be a subset of the
    /// package's declared permissions (validated at admission): a hook
    /// can never widen the package's permission set (ECO-07).
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// Activation state of an installed plugin package. Installation never
/// implies activation (ECO-04): a package enters `Installed` and stays
/// inert — nothing loaded, nothing run — until an explicit operator action
/// moves it. A misbehaving package can be suspended without uninstalling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActivation {
    /// Installed but inert: nothing is loaded, nothing runs.
    #[default]
    Installed,
    /// Explicitly enabled: components may be exercised.
    Active,
    /// Explicitly disabled after being active.
    Disabled,
    /// Quarantined after misbehavior; nothing runs until unquarantined.
    Quarantined,
}

impl PluginActivation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Quarantined => "quarantined",
        }
    }
}

/// Where a skill came from. Provenance is recorded so a skill is always
/// attributable; it never grants authority by itself (ECO-01/ECO-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Ships with the runtime (first-party, operator-reviewed).
    Builtin,
    /// Contributed by an installed plugin package.
    Package,
    /// Installed directly by an operator, outside any package.
    Operator,
}

/// Activation state of a declared skill. Metadata only: the runtime never
/// executes a skill, so activation records the operator's intent (which
/// skills may be offered) without turning instructions into runtime
/// authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivation {
    /// Declared but not offered.
    #[default]
    Inactive,
    /// Declared and offered for use.
    Active,
}

/// A declared skill: versioned procedural knowledge built from existing
/// tools. Metadata only (ECO-01): never executed, never injected into
/// context, adds no authority. The referenced instructions, when they are
/// offered, enter context as ordinary (non-System-authority) content and
/// only while the skill is active (ECO-06).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDeclaration {
    /// Component id (same grammar as a capability id).
    pub id: String,
    /// Component version, e.g. "1.0.0".
    pub version: String,
    /// One-line purpose.
    pub summary: String,
    /// Where the procedure lives (a package-relative path). Shape-checked
    /// at admission; the runtime does not read it in v0.
    pub reference: String,
    /// Where the skill came from; recorded for attribution, never a
    /// permission source.
    pub provenance: SkillSource,
    /// Whether the skill is offered. Metadata only (no runtime effect).
    #[serde(default)]
    pub activation: SkillActivation,
}

/// A declared adapter (e.g. an MCP endpoint). Metadata only until the
/// adapter plane (ECO-05) interprets it; installing one must not inject
/// its schema catalog into every request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDeclaration {
    pub id: String,
    /// The adapter protocol, e.g. `mcp`. v0 reserves the name; nothing
    /// connects yet.
    pub protocol: String,
    /// Where the adapter is reached (a command, socket or URL). Bounded
    /// and shape-checked; not contacted in v0.
    pub endpoint: String,
}

/// A package dependency: another package id plus a version range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageDependency {
    pub id: String,
    pub range: VersionRange,
}

/// A declared self-check: a bounded argv command run inside the sandbox at
/// install/test time (ECO-04). The core never runs it during a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestDeclaration {
    pub id: String,
    /// Program and arguments, passed verbatim — no shell parsing.
    pub command: Vec<String>,
}

/// One versioned, installable plugin package. `ToolSpec` carries the
/// schema value, so the manifest itself is cloneable/serializable but not
/// equality-comparable; component declarations below are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPackageManifest {
    /// Package id (same grammar as a capability id).
    pub id: String,
    /// Package version, e.g. "1.2.0".
    pub version: String,
    pub name: String,
    pub summary: String,
    /// Compatibility range of the runtime contract this package targets
    /// (the framed protocol / core contract version).
    pub api: VersionRange,
    /// Contributed tool schemas, same shape and limits as capability tools.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Declared skills (metadata only, ECO-01).
    #[serde(default)]
    pub skills: Vec<SkillDeclaration>,
    /// Declared hooks (metadata only, ECO-01).
    #[serde(default)]
    pub hooks: Vec<HookDeclaration>,
    /// Declared adapters (metadata only until ECO-05).
    #[serde(default)]
    pub adapters: Vec<AdapterDeclaration>,
    /// Package dependencies (shape-validated now, resolved at install).
    #[serde(default)]
    pub dependencies: Vec<PackageDependency>,
    /// Declared permissions from the known-word table.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Declared self-checks, run in a sandbox at install/test time.
    #[serde(default)]
    pub tests: Vec<TestDeclaration>,
}

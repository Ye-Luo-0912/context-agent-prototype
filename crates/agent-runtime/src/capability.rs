//! The dynamic capability plane of the runtime: a shared registry that
//! accepts capabilities at composition time or at runtime, and a tool
//! dispatcher that merges the trusted core's tools with the capabilities'
//! tools under one unified lifecycle — `capability.search` / `inspect` /
//! `load` / `unload` cover both, and a capability's tools only enter the
//! model surface when they are loaded (explicitly or by the runtime), so
//! the prompt does not grow with every registered capability.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use agent_contracts::{
    AgentError, AgentResult, ArtifactHandle, CAPABILITY_MANAGE, CAPABILITY_SEARCH_DEFAULT_LIMIT,
    CAPABILITY_SEARCH_MAX_LIMIT, Capability, CapabilityActivation, CapabilityInvocationContext,
    CapabilityLifecycle, CapabilityManifest, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, Effect, PROCESS_RUN, RUNTIME_CONTEXT_CONTROL, ToolCatalogEntry,
    ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolOutput, ToolRisk,
    ToolSpec, ToolSurfaceSnapshot, WORKSPACE_READ, WORKSPACE_WRITE, WorkspaceHandle,
    is_known_permission, validate_capability_id,
};
use agent_workspace::{ArtifactStoreHandle, ConfinedWorkspaceHandle, Workspace};
use async_trait::async_trait;
use serde_json::json;

/// The asynchronous lifecycle of a capability, from registration through
/// start/stop. Transitions are serialized per capability: a per-capability
/// async `run_lock` guards the transition, so concurrent `ensure_started`
/// calls cannot double-start, and a stop cannot race an in-flight start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRunState {
    /// Registered but never started (or stopped since).
    Stopped,
    /// A `start()` is in flight.
    Starting,
    /// `start()` returned Ok; the capability is running.
    Started,
    /// A `stop()` is in flight.
    Stopping,
    /// The last start/stop transition failed; a later start may retry.
    Failed,
}

impl CapabilityRunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityRunState::Stopped => "stopped",
            CapabilityRunState::Starting => "starting",
            CapabilityRunState::Started => "started",
            CapabilityRunState::Stopping => "stopping",
            CapabilityRunState::Failed => "failed",
        }
    }
}

struct Entry {
    capability: Arc<dyn Capability>,
    /// Manifest snapshot captured once at registration. The registry never
    /// calls back into the capability object while holding its lock: a slow
    /// or re-entrant `manifest()`/`tool_specs()` must not stall (or deadlock)
    /// a catalog read. Registration validates and caches both, and every
    /// later query reads the cache.
    manifest: CapabilityManifest,
    tool_specs: Vec<ToolSpec>,
    /// Lifecycle state. The lock is held across the async `start()`/`stop()`
    /// call, so a capability's start/stop must not re-enter the registry
    /// for the same capability (that would deadlock).
    run_state: CapabilityRunState,
    run_lock: Arc<tokio::sync::Mutex<()>>,
    /// The effective maturity. External (out-of-process) capabilities are
    /// pinned to Experimental regardless of their declared status, so an LLM
    /// cannot promote its own module to Stable.
    status: CapabilityStatus,
    /// Whether the runtime will run this capability at all. External
    /// capabilities enter `Disabled`; only an explicit enable (operator or
    /// evaluator) makes them usable.
    activation: CapabilityActivation,
    /// Which tools of this capability are on the model surface. Registration
    /// alone keeps them `Available`; `capability.load` (or the runtime) puts
    /// exactly the named tool on the surface — sibling tools of the same
    /// capability stay off until they are loaded individually — and
    /// `capability.unload` takes one tool off.
    loaded_tools: HashSet<String>,
    /// A tool of this capability is executing right now.
    active: bool,
}

/// Registration limits for capability-declared tool schemas: a single
/// capability must not be able to grow the model surface without bound — a
/// huge schema, a huge description or a huge tool count is itself context
/// pollution. The limits are enforced at registration (validated once, then
/// cached), so a runaway capability is rejected before it ever reaches the
/// catalog.
pub const MAX_TOOLS_PER_CAPABILITY: usize = 32;
pub const MAX_TOOL_NAME_CHARS: usize = 64;
pub const MAX_TOOL_DESCRIPTION_CHARS: usize = 200;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 4 * 1024;

/// Validate the tool schemas a capability declares at registration: name
/// shape/length, description length, per-schema byte size, duplicate names
/// within the capability, and the per-capability tool count.
fn validate_tool_specs(manifest_id: &str, specs: &[ToolSpec]) -> AgentResult<()> {
    if specs.len() > MAX_TOOLS_PER_CAPABILITY {
        return Err(AgentError::InvalidRequest(format!(
            "capability '{manifest_id}' declares {} tools, above the {MAX_TOOLS_PER_CAPABILITY} per-capability cap",
            specs.len()
        )));
    }
    let mut names = std::collections::HashSet::new();
    for spec in specs {
        if spec.name.is_empty() || spec.name.len() > MAX_TOOL_NAME_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares a tool name of {} chars (allowed 1..={MAX_TOOL_NAME_CHARS})",
                spec.name.len()
            )));
        }
        let well_formed = spec
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
        if !well_formed {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares tool name '{}': only [A-Za-z0-9._:-] are allowed",
                spec.name
            )));
        }
        if !names.insert(spec.name.clone()) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' declares tool '{name}' twice",
                name = spec.name
            )));
        }
        if spec.description.len() > MAX_TOOL_DESCRIPTION_CHARS {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' tool '{}' description is {} chars, above the {MAX_TOOL_DESCRIPTION_CHARS} cap",
                spec.name,
                spec.description.len()
            )));
        }
        let bytes = serde_json::to_vec(&spec.input_schema)
            .unwrap_or_default()
            .len();
        if bytes > MAX_TOOL_SCHEMA_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{manifest_id}' tool '{}' input schema is {bytes} bytes, above the {MAX_TOOL_SCHEMA_BYTES} cap",
                spec.name
            )));
        }
    }
    Ok(())
}

/// Validate the authority a manifest declares against what the runtime will
/// actually enforce. The approval gate auto-allows `ReadOnly` tools, so the
/// risk label must be *derived* from the declared authority, never
/// self-declared by a side-effecting capability:
///
/// - every declared permission must be a known permission string (unknown
///   access is denied by refusing the declaration);
/// - a capability that declares any side-effecting permission may not mark
///   any tool `ReadOnly` (a process that can write must not auto-allow);
/// - a tool's risk may not exceed its grant (a `WorkspaceWrite` tool needs
///   `workspace:write`, a `ProcessExecution` tool needs `process:run`);
/// - a process-transport capability may declare `workspace:write` because
///   the wire effect broker stages its mutations: the adapter validates the
///   child's structured wire effects against the grant and commits them
///   through the confined handle behind the generation fence.
fn validate_manifest_authority(
    manifest: &CapabilityManifest,
    tool_specs: &[ToolSpec],
) -> AgentResult<()> {
    for permission in &manifest.permissions {
        if !is_known_permission(permission) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' declares unknown permission '{permission}'; allowed: workspace:read, workspace:write, process:run, runtime:context-control, artifact:*",
                manifest.id
            )));
        }
    }
    let declares_approval_gated_mutation = manifest
        .permissions
        .iter()
        .any(|p| p == WORKSPACE_WRITE || p == PROCESS_RUN);
    if declares_approval_gated_mutation {
        for spec in tool_specs {
            if spec.risk == ToolRisk::ReadOnly {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares workspace-write/process-run authority but tool '{}' self-declares ReadOnly; risk is derived from declared authority, never self-declared (ReadOnly auto-allows at the approval gate)",
                    manifest.id, spec.name
                )));
            }
        }
    }
    for spec in tool_specs {
        match spec.risk {
            ToolRisk::WorkspaceWrite => {
                if !manifest.permissions.iter().any(|p| p == WORKSPACE_WRITE) {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{}' tool '{}' needs the '{WORKSPACE_WRITE}' permission, which is not declared",
                        manifest.id, spec.name
                    )));
                }
            }
            ToolRisk::ProcessExecution => {
                if !manifest.permissions.iter().any(|p| p == PROCESS_RUN) {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{}' tool '{}' needs the '{PROCESS_RUN}' permission, which is not declared",
                        manifest.id, spec.name
                    )));
                }
            }
            ToolRisk::ReadOnly => {}
        }
    }
    // A process capability may declare `workspace:write` — but only because
    // the wire effect broker exists: the child stages structured wire
    // effects and the adapter commits them through the confined workspace
    // handle behind the generation fence. The child itself never writes.
    // Enforcement of the write path is the adapter's job; a process whose
    // adapter is not the wire-brokering one simply cannot be registered
    // (there is only one adapter).
    Ok(())
}

/// One row of the platform's capability catalog (the discovery surface).
#[derive(Debug, Clone)]
pub struct CapabilityCatalogEntry {
    pub id: String,
    pub status: CapabilityStatus,
    pub activation: CapabilityActivation,
    pub transport: CapabilityTransport,
    pub tools: Vec<String>,
    pub run_state: CapabilityRunState,
}

/// Runtime-mutable registry of dynamic capabilities, shared between the
/// module host (registration) and the tool dispatcher (specs + routing).
/// Registration is not gated on the host lifecycle: a capability can be
/// published mid-run and its tools appear on the next model request.
#[derive(Default)]
pub struct CapabilityRegistry {
    /// Coordinates a unified snapshot with surface mutations without holding
    /// `inner` across the lower dispatcher's `snapshot()` callback. Writers
    /// always acquire this gate before `inner`; readers may consequently
    /// freeze the capability half while the independent base snapshot is
    /// captured at one common linearization point.
    surface_gate: RwLock<()>,
    inner: RwLock<HashMap<String, Entry>>,
    /// Tool names the runtime owns (builtin core tools plus the unified
    /// control tools). A capability may never shadow them: routing would
    /// otherwise be hijackable by declaration.
    reserved: RwLock<HashSet<String>>,
    /// The registry's surface generation: bumped on every capability
    /// surface change (register / activation / load / unload), so the
    /// unified dispatcher snapshot's `generation` reflects dynamic
    /// capability changes — not just the builtin catalog's.
    generation: AtomicU64,
    /// Derived catalog metadata (`catalog_rows`) is cached and invalidated
    /// by this counter: bumped on *every* mutation that can change what the
    /// discovery rows report (register, activation, load/unload, active
    /// marks, restore). Distinct from `generation`, which is the audit
    /// surface generation — a tool executing mid-round must not churn the
    /// snapshot's audit identity.
    catalog_version: AtomicU64,
    /// Cached unified discovery rows, keyed on `catalog_version`: an
    /// unchanged catalog answers `capability.search` without rebuilding
    /// every row from the registry lock.
    rows_cache: RwLock<Option<(u64, Arc<Vec<ToolCatalogEntry>>)>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry's surface generation: any capability surface change
    /// (register / activation / load / unload) bumps it, so an auditor can
    /// tell that a dynamic capability changed the model surface.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Claim tool names the runtime owns. Called by the tool dispatcher
    /// with the builtin catalog plus its control tools, so `register` can
    /// reject a capability that tries to shadow them.
    pub fn reserve_names(&self, names: impl IntoIterator<Item = String>) {
        self.reserved
            .write()
            .expect("capability registry poisoned")
            .extend(names);
    }

    /// Register a capability. Rejects duplicate ids, missing declared
    /// dependencies, tool names that shadow the runtime's own tools, and
    /// tool names already owned by another capability — the model must
    /// never see a half-wired tool or an ambiguous route. The manifest and
    /// tool schemas are read and validated *once*, before the lock, and
    /// cached on the entry: the registry never calls back into the
    /// capability object while holding its lock.
    pub fn register(&self, capability: Arc<dyn Capability>) -> AgentResult<()> {
        // Lock-free validation up front: manifest + tool schemas are read
        // exactly once per registration and cached. A slow, re-entrant or
        // panicking capability implementation can only do so at register
        // time — never under the registry's lock.
        let manifest = capability.manifest().clone();
        let tool_specs = capability.tool_specs();
        // The id is identity: it is validated before anything derived from
        // it (tool names, routes, directories).
        validate_capability_id(&manifest.id).map_err(AgentError::InvalidRequest)?;
        validate_tool_specs(&manifest.id, &tool_specs)?;
        validate_manifest_authority(&manifest, &tool_specs)?;
        let tool_names: Vec<&str> = tool_specs.iter().map(|spec| spec.name.as_str()).collect();

        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        if inner.contains_key(&manifest.id) {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{}' is already registered",
                manifest.id
            )));
        }
        for requirement in &manifest.requires {
            if !inner.contains_key(requirement) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' requires '{}' which is not registered",
                    manifest.id, requirement
                )));
            }
        }
        let reserved = self.reserved.read().expect("capability registry poisoned");
        for name in &tool_names {
            if reserved.contains(*name) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares tool '{name}', which is reserved by the runtime; capabilities cannot shadow core tools",
                    manifest.id
                )));
            }
        }
        drop(reserved);
        for name in &tool_names {
            if let Some((owner, _)) = inner.iter().find(|(_, entry)| {
                entry
                    .tool_specs
                    .iter()
                    .any(|spec| spec.name.as_str() == *name)
            }) {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{}' declares tool '{name}', which is already owned by capability '{owner}'",
                    manifest.id
                )));
            }
        }
        // The maturity ladder is climbed, not declared: out-of-process
        // (external/LLM-authored) capabilities always start Experimental.
        let status = if manifest.transport != CapabilityTransport::Builtin
            && manifest.status != CapabilityStatus::Experimental
        {
            CapabilityStatus::Experimental
        } else {
            manifest.status
        };
        // Activation is granted, not declared: only the trusted in-process
        // core is usable immediately; external capabilities enter Disabled
        // and need an explicit enable before anything runs.
        let activation = if manifest.transport == CapabilityTransport::Builtin {
            CapabilityActivation::Enabled
        } else {
            CapabilityActivation::Disabled
        };
        inner.insert(
            manifest.id.clone(),
            Entry {
                capability,
                manifest,
                tool_specs,
                run_state: CapabilityRunState::Stopped,
                run_lock: Arc::new(tokio::sync::Mutex::new(())),
                status,
                activation,
                loaded_tools: HashSet::new(),
                active: false,
            },
        );
        // A new capability changes the catalog; surface-related flags are
        // covered by their own bumps below.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// The effective maturity of a registered capability (Experimental for
    /// external capabilities regardless of declaration).
    pub fn status(&self, id: &str) -> Option<CapabilityStatus> {
        self.inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .map(|entry| entry.status)
    }

    /// The current activation of a registered capability.
    pub fn activation(&self, id: &str) -> Option<CapabilityActivation> {
        self.inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .map(|entry| entry.activation)
    }

    /// Set a capability's activation: `Enabled` makes it loadable and
    /// invocable, `Disabled`/`Quarantined` take its tools off the model
    /// surface and block further calls. Enabling is the operator/evaluator
    /// action that external capabilities wait for.
    pub fn set_activation(&self, id: &str, activation: CapabilityActivation) -> AgentResult<()> {
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let entry = inner.get_mut(id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
        })?;
        entry.activation = activation;
        // A capability that cannot run must not keep its tools on the
        // model surface.
        if !activation.usable() {
            entry.loaded_tools.clear();
        }
        // Activation flips the usable surface: bump the generation.
        self.generation.fetch_add(1, Ordering::Relaxed);
        // ... and the derived discovery rows.
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Convenience: enable a capability (usable from now on).
    pub fn enable(&self, id: &str) -> AgentResult<()> {
        self.set_activation(id, CapabilityActivation::Enabled)
    }

    /// Convenience: disable a capability (registered, but not usable).
    pub fn disable(&self, id: &str) -> AgentResult<()> {
        self.set_activation(id, CapabilityActivation::Disabled)
    }

    /// Convenience: quarantine a capability after misbehavior.
    pub fn quarantine(&self, id: &str) -> AgentResult<()> {
        self.set_activation(id, CapabilityActivation::Quarantined)
    }

    /// Snapshot of every registered capability, for the discovery surface.
    pub fn catalog(&self) -> Vec<CapabilityCatalogEntry> {
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .values()
            .map(|entry| {
                let manifest = &entry.manifest;
                CapabilityCatalogEntry {
                    id: manifest.id.clone(),
                    status: entry.status,
                    activation: entry.activation,
                    transport: manifest.transport.clone(),
                    tools: entry.tool_specs.iter().map(|s| s.name.clone()).collect(),
                    run_state: entry.run_state,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// The tool schemas of *loaded and usable* capability tools only — the
    /// model surface. Unloaded or disabled capabilities stay registered but
    /// invisible, so the prompt does not grow with every registered
    /// capability and a suspended one cannot linger on the surface. Loading
    /// one tool of a capability never surfaces its siblings.
    pub fn loaded_tool_specs(&self) -> Vec<ToolSpec> {
        self.loaded_surface().0
    }

    /// Loaded schemas and their exact registry generation. Surface writers
    /// bump the generation before releasing `inner`, so this single read
    /// lock pairs the two values atomically.
    fn loaded_surface(&self) -> (Vec<ToolSpec>, u64) {
        let inner = self.inner.read().expect("capability registry poisoned");
        let specs = inner
            .values()
            .filter(|entry| entry.activation.usable() && !entry.loaded_tools.is_empty())
            .flat_map(|entry| {
                entry
                    .tool_specs
                    .iter()
                    .filter(|spec| entry.loaded_tools.contains(&spec.name))
                    .cloned()
            })
            .collect();
        let generation = self.generation.load(Ordering::Relaxed);
        (specs, generation)
    }

    /// The capability that owns a tool name, if any.
    pub fn by_tool(&self, tool_name: &str) -> Option<Arc<dyn Capability>> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .tool_specs
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| entry.capability.clone())
        })
    }

    /// The capability id owning a tool name.
    pub fn owner_of(&self, tool_name: &str) -> Option<String> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.iter().find_map(|(id, entry)| {
            entry
                .tool_specs
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| id.clone())
        })
    }

    /// Unified `capability.load`: put exactly one tool of the owning
    /// capability on the model surface. Sibling tools of the same
    /// capability stay off until they are loaded individually — a single
    /// tool load never surfaces the whole capability. Unknown tool names
    /// are rejected like the builtin catalog does, and a
    /// disabled/quarantined capability cannot load — activation is the
    /// gate in front of the surface.
    pub fn load_tool(&self, tool_name: &str) -> AgentResult<()> {
        let owner = self
            .owner_of(tool_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let entry = inner
            .get_mut(&owner)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        if !entry.activation.usable() {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{owner}' is {}; enable it before loading its tools",
                entry.activation.as_str()
            )));
        }
        entry.loaded_tools.insert(tool_name.to_string());
        // A load puts tools on the surface: bump the generation.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Unified `capability.unload`: take one tool of the owning capability
    /// off the model surface. Siblings stay loaded.
    pub fn unload_tool(&self, tool_name: &str) -> AgentResult<()> {
        let owner = self
            .owner_of(tool_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        if let Some(entry) = inner.get_mut(&owner) {
            entry.loaded_tools.remove(tool_name);
        }
        // An unload takes tools off the surface: bump the generation.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Lifecycle state of one capability tool: `Active` while executing,
    /// `Loaded` when that tool itself is on the surface, `Available`
    /// otherwise. Sibling tools of the same capability report `Available`
    /// until they are loaded individually. A disabled/quarantined
    /// capability reports `Available` regardless — its tools are not on
    /// the surface.
    pub fn tool_state(&self, tool_name: &str) -> Option<ToolLifecycle> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .tool_specs
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| {
                    if entry.active {
                        ToolLifecycle::Active
                    } else if entry.loaded_tools.contains(tool_name) && entry.activation.usable() {
                        ToolLifecycle::Loaded
                    } else {
                        ToolLifecycle::Available
                    }
                })
        })
    }

    /// The full spec of one capability tool (for `capability.inspect`).
    pub fn tool_spec(&self, tool_name: &str) -> Option<ToolSpec> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .tool_specs
                .iter()
                .find(|spec| spec.name == tool_name)
                .cloned()
        })
    }

    /// Unified discovery rows: every capability tool with its owner id and
    /// lifecycle state. The rows are derived metadata, cached across calls
    /// and rebuilt only when `catalog_version` changes — an unchanged
    /// catalog answers `capability.search` with an `Arc` clone instead of
    /// re-reading the registry and re-cloning every tool description.
    pub fn catalog_rows(&self) -> Arc<Vec<ToolCatalogEntry>> {
        let version = self.catalog_version.load(Ordering::Relaxed);
        if let Some((cached_version, rows)) = self.rows_cache.read().expect("poisoned").as_ref()
            && *cached_version == version
        {
            return rows.clone();
        }
        let rows = {
            let inner = self.inner.read().expect("capability registry poisoned");
            let mut rows = Vec::new();
            for (id, entry) in inner.iter() {
                let owner = id.clone();
                let active = entry.active;
                let usable = entry.activation.usable();
                for spec in &entry.tool_specs {
                    let state = if active {
                        ToolLifecycle::Active
                    } else if entry.loaded_tools.contains(&spec.name) && usable {
                        ToolLifecycle::Loaded
                    } else {
                        ToolLifecycle::Available
                    };
                    rows.push(ToolCatalogEntry {
                        name: spec.name.clone(),
                        state,
                        owner: owner.clone(),
                        description: spec.description.clone(),
                    });
                }
            }
            rows.sort_by(|a, b| a.name.cmp(&b.name));
            rows
        };
        let rows = Arc::new(rows);
        *self
            .rows_cache
            .write()
            .expect("capability registry poisoned") = Some((version, rows.clone()));
        rows
    }

    /// Mark a capability tool as executing (Active until `mark_idle`).
    pub fn mark_active(&self, tool_name: &str) {
        let owner = self.owner_of(tool_name);
        if let Some(owner) = owner
            && let Some(entry) = self
                .inner
                .write()
                .expect("capability registry poisoned")
                .get_mut(&owner)
        {
            entry.active = true;
        }
        // The discovery rows carry the Active state: invalidate the cache.
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear the executing marker after a call finished.
    pub fn mark_idle(&self, tool_name: &str) {
        let owner = self.owner_of(tool_name);
        if let Some(owner) = owner
            && let Some(entry) = self
                .inner
                .write()
                .expect("capability registry poisoned")
                .get_mut(&owner)
        {
            entry.active = false;
        }
        // The discovery rows carry the Active state: invalidate the cache.
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot of every registered capability's surface state (activation +
    /// loaded tools), for checkpoints. Registration identity itself is not
    /// part of the snapshot: capabilities are re-registered by the
    /// composition root on a fresh run, then this re-applies their flags.
    pub fn snapshot(&self) -> Vec<crate::checkpoint::CapabilitySnapshot> {
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .iter()
            .map(|(id, entry)| {
                let mut loaded_tools: Vec<String> = entry.loaded_tools.iter().cloned().collect();
                loaded_tools.sort();
                crate::checkpoint::CapabilitySnapshot {
                    id: id.clone(),
                    activation: entry.activation,
                    // The legacy whole-capability flag mirrors "at least one
                    // tool loaded" so older readers still see a non-empty
                    // surface; the per-tool list is authoritative.
                    loaded: !loaded_tools.is_empty(),
                    loaded_tools,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// Re-apply a checkpoint's capability surface state: activation first,
    /// then the loaded tools (a loaded flag without a usable activation is
    /// dropped — activation is the gate in front of the model surface).
    /// Old whole-capability checkpoints (`loaded: true` with no tool list)
    /// migrate to "every declared tool loaded".
    pub fn restore(&self, state: &[crate::checkpoint::CapabilitySnapshot]) {
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        for entry in state {
            let Some(current) = inner.get_mut(&entry.id) else {
                // The capability is not registered in this run; its flags
                // have nothing to apply to.
                continue;
            };
            current.activation = entry.activation;
            if !entry.loaded_tools.is_empty() {
                // Per-tool format: only the named tools go on the surface,
                // and only those the capability actually declares in this
                // run (names unknown here are dropped).
                current.loaded_tools = entry
                    .loaded_tools
                    .iter()
                    .filter(|name| {
                        current
                            .tool_specs
                            .iter()
                            .any(|spec| spec.name.as_str() == *name)
                    })
                    .cloned()
                    .collect();
            } else if entry.loaded {
                // Legacy whole-capability format: everything was on the
                // surface in the checkpoint.
                current.loaded_tools = current
                    .tool_specs
                    .iter()
                    .map(|spec| spec.name.clone())
                    .collect();
            } else {
                current.loaded_tools.clear();
            }
            // A loaded flag without a usable activation is dropped —
            // activation is the gate in front of the model surface.
            if !current.activation.usable() {
                current.loaded_tools.clear();
            }
        }
        // Restore changes the model-visible surface just like explicit
        // activation/load operations, so both the audit generation and the
        // derived discovery rows must advance.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Start every eager capability that has not started yet (host start).
    /// Disabled/quarantined capabilities are not started — they are not
    /// usable, so there is nothing to run.
    pub async fn start_eager(&self) -> AgentResult<()> {
        let ids: Vec<String> = {
            let inner = self.inner.read().expect("capability registry poisoned");
            inner
                .iter()
                .filter(|(_, entry)| {
                    entry.activation.usable()
                        && entry.manifest.lifecycle == CapabilityLifecycle::Eager
                        && matches!(
                            entry.run_state,
                            CapabilityRunState::Stopped | CapabilityRunState::Failed
                        )
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.ensure_started(&id).await?;
        }
        Ok(())
    }

    /// Start a capability on first use (lazy lifecycle) and drive it to
    /// `Started`. A disabled/quarantined capability is rejected: the start
    /// is the point where "not usable" would otherwise turn into a running
    /// process.
    ///
    /// The transition is serialized per capability: a second concurrent
    /// caller either observes `Started` (and returns immediately) or waits
    /// for the in-flight transition to finish, then re-checks — it never
    /// issues a second `start()`. The per-capability lock is held across the
    /// async `start()` call, so the capability's `start` must not re-enter
    /// the registry for the same capability.
    pub async fn ensure_started(&self, id: &str) -> AgentResult<()> {
        // Fast path: already running — no lock round-trip.
        if self
            .inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .is_some_and(|entry| entry.run_state == CapabilityRunState::Started)
        {
            return Ok(());
        }
        let run_lock = {
            let inner = self.inner.read().expect("capability registry poisoned");
            inner
                .get(id)
                .ok_or_else(|| {
                    AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
                })?
                .run_lock
                .clone()
        };
        // Serialize the transition: only one caller drives
        // Stopped/Failed -> Starting -> Started/Failed at a time.
        let _guard = run_lock.lock().await;
        let capability = {
            let inner = self.inner.read().expect("capability registry poisoned");
            let entry = inner.get(id).ok_or_else(|| {
                AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
            })?;
            match entry.run_state {
                CapabilityRunState::Started => return Ok(()),
                // Unreachable while holding the run lock (the other caller
                // finished before we acquired it), kept defensive.
                CapabilityRunState::Starting | CapabilityRunState::Stopping => {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{id}' is {}; cannot start now",
                        entry.run_state.as_str()
                    )));
                }
                CapabilityRunState::Stopped | CapabilityRunState::Failed => {}
            }
            if !entry.activation.usable() {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{id}' is {}; enable it before use",
                    entry.activation.as_str()
                )));
            }
            entry.capability.clone()
        };
        {
            let mut inner = self.inner.write().expect("capability registry poisoned");
            if let Some(entry) = inner.get_mut(id) {
                entry.run_state = CapabilityRunState::Starting;
            }
        }
        match capability.start().await {
            Ok(()) => {
                let mut inner = self.inner.write().expect("capability registry poisoned");
                if let Some(entry) = inner.get_mut(id) {
                    entry.run_state = CapabilityRunState::Started;
                }
                Ok(())
            }
            Err(error) => {
                let mut inner = self.inner.write().expect("capability registry poisoned");
                if let Some(entry) = inner.get_mut(id) {
                    entry.run_state = CapabilityRunState::Failed;
                }
                Err(error)
            }
        }
    }

    /// Stop every capability and drive it back to `Stopped` (host stop).
    /// Best effort: every capability gets its stop call even when an
    /// earlier one fails, and all errors are aggregated into one result.
    /// Each stop transition takes the per-capability run lock, so it cannot
    /// race an in-flight start of the same capability; a stop failure
    /// leaves the capability `Failed` (observable, retryable).
    pub async fn stop_all(&self) -> AgentResult<()> {
        let ids: Vec<String> = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .keys()
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            let run_lock = {
                let inner = self.inner.read().expect("capability registry poisoned");
                match inner.get(&id) {
                    Some(entry) => entry.run_lock.clone(),
                    None => continue,
                }
            };
            let _guard = run_lock.lock().await;
            let capability = {
                let inner = self.inner.read().expect("capability registry poisoned");
                match inner.get(&id) {
                    Some(entry) => entry.capability.clone(),
                    None => continue,
                }
            };
            {
                let mut inner = self.inner.write().expect("capability registry poisoned");
                if let Some(entry) = inner.get_mut(&id) {
                    entry.run_state = CapabilityRunState::Stopping;
                }
            }
            match capability.stop().await {
                Ok(()) => {
                    let mut inner = self.inner.write().expect("capability registry poisoned");
                    if let Some(entry) = inner.get_mut(&id) {
                        entry.run_state = CapabilityRunState::Stopped;
                    }
                }
                Err(error) => {
                    let mut inner = self.inner.write().expect("capability registry poisoned");
                    if let Some(entry) = inner.get_mut(&id) {
                        entry.run_state = CapabilityRunState::Failed;
                    }
                    errors.push(error);
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(aggregate_errors(errors))
        }
    }
}

/// Join multiple errors into one message so a best-effort teardown can
/// report every failure instead of the first one it hit.
fn aggregate_errors(errors: Vec<AgentError>) -> AgentError {
    let message = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    AgentError::Internal(message)
}

/// A `ToolDispatcher` that merges the trusted core's tools with the dynamic
/// capabilities' tools under one unified lifecycle. The kernel keeps talking
/// to one `ToolDispatcher`; capabilities are registered against the shared
/// registry at runtime. The unified control tools (`capability.search` /
/// `capability.inspect` / `capability.load` / `capability.unload`) are
/// provided here — they cover the builtin catalog *and* the capability
/// registry, and the builtin's own control tools are filtered out of the
/// surface to avoid duplicates.
pub struct CapabilityAwareDispatcher {
    base: Arc<dyn ToolDispatcher>,
    capabilities: Arc<CapabilityRegistry>,
    /// The workspace capabilities may touch, if the composition root wired
    /// one in. Capabilities never receive this directly — every invocation
    /// gets a `CapabilityInvocationContext` whose confined handles are
    /// built from it.
    workspace: Option<Arc<Workspace>>,
}

impl CapabilityAwareDispatcher {
    pub fn new(base: Arc<dyn ToolDispatcher>, capabilities: Arc<CapabilityRegistry>) -> Self {
        Self::with_workspace(base, capabilities, None)
    }

    /// Constructor with a workspace: capabilities that declare
    /// `workspace:*` / artifact permissions receive confined handles into
    /// it inside their invocation context. Without it, those permissions
    /// are granted as declarations only (nothing to touch).
    pub fn with_workspace(
        base: Arc<dyn ToolDispatcher>,
        capabilities: Arc<CapabilityRegistry>,
        workspace: Option<Arc<Workspace>>,
    ) -> Self {
        // The runtime owns the builtin tool names and the control tools;
        // capabilities may never shadow them (registration rejects them).
        let mut reserved: Vec<String> =
            base.catalog().into_iter().map(|entry| entry.name).collect();
        reserved.push(CAPABILITY_MANAGE.to_string());
        capabilities.reserve_names(reserved);
        Self {
            base,
            capabilities,
            workspace,
        }
    }

    /// The unified control tool schemas (always visible).
    fn control_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: CAPABILITY_MANAGE.into(),
                description: "Manage the tool catalog in one call (builtin tools and dynamic capabilities). ops: search (list known tools with lifecycle state and owner), inspect (one tool's schema, risk, owner and state), load (put one tool on the model surface; its capability's sibling tools stay off until loaded individually), unload (take it off; core builtin tools cannot be unloaded).".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["op"],
                    "properties": {
                        "op": {"type": "string", "enum": ["search", "inspect", "load", "unload"]},
                        "name": {"type": "string", "description": "Tool name for inspect/load/unload"},
                        "query": {"type": "string", "description": "search: optional name filter"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "search: max rows in the model-facing page (default 20)"},
                        "cursor": {"type": "string", "description": "search: last tool name of the previous page"}
                    }
                }),
                risk: agent_contracts::ToolRisk::ReadOnly,
            },
        ]
    }

    /// The unified catalog: builtin rows plus capability rows, sorted by name.
    fn unified_catalog(&self) -> Vec<ToolCatalogEntry> {
        let mut rows = self.base.catalog();
        rows.extend(self.capabilities.catalog_rows().iter().cloned());
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Mutex, mpsc},
        thread,
        time::Duration,
    };

    struct BlockingBase {
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    #[async_trait::async_trait]
    impl ToolDispatcher for BlockingBase {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "base.test".into(),
                description: "base test tool".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            }]
        }

        fn snapshot(&self) -> ToolSurfaceSnapshot {
            self.entered
                .lock()
                .expect("entered lock poisoned")
                .take()
                .expect("snapshot is called once")
                .send(())
                .expect("test receiver dropped");
            self.release
                .lock()
                .expect("release lock poisoned")
                .recv()
                .expect("test sender dropped");
            ToolSurfaceSnapshot {
                specs: self.specs(),
                generation: 41,
                source_revisions: agent_contracts::ToolSurfaceSourceRevisions {
                    builtin_catalog_generation: 41,
                    ..Default::default()
                },
                ..ToolSurfaceSnapshot::default()
            }
        }

        async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            unreachable!("this dispatcher is snapshot-only")
        }
    }

    #[test]
    fn unified_snapshot_fences_capability_mutation_while_base_is_captured() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let registry = Arc::new(CapabilityRegistry::new());
        let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
            Arc::new(BlockingBase {
                entered: Mutex::new(Some(entered_tx)),
                release: Mutex::new(release_rx),
            }),
            registry.clone(),
        ));

        let snapshot_thread = thread::spawn(move || dispatcher.snapshot());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("base snapshot was not entered");

        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mutation_registry = registry.clone();
        let mutation_thread = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            // Restore is a surface mutation even when this empty test
            // registry has no matching capability entries.
            mutation_registry.restore(&[]);
            finished_tx.send(()).unwrap();
        });
        attempted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("mutation thread did not start");
        let finished_early = finished_rx.recv_timeout(Duration::from_millis(100)).is_ok();

        release_tx.send(()).unwrap();
        let snapshot = snapshot_thread.join().unwrap();
        if !finished_early {
            finished_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("mutation did not resume after snapshot");
        }
        mutation_thread.join().unwrap();

        assert!(
            !finished_early,
            "a capability surface mutation crossed the unified snapshot"
        );
        assert_eq!(snapshot.source_revisions.builtin_catalog_generation, 41);
        assert_eq!(snapshot.source_revisions.capability_catalog_generation, 0);
        assert_eq!(snapshot.generation, 41);
        assert_eq!(registry.generation(), 1);
    }

    /// A small in-process capability with three tools, so a single-tool
    /// load can prove siblings stay off the surface.
    struct DemoCapability {
        manifest: CapabilityManifest,
    }

    #[async_trait::async_trait]
    impl Capability for DemoCapability {
        fn manifest(&self) -> &CapabilityManifest {
            &self.manifest
        }

        async fn invoke(
            &self,
            _call: agent_contracts::ToolCall,
            _ctx: CapabilityInvocationContext,
        ) -> AgentResult<CapabilityOutcome> {
            unreachable!("surface tests never invoke")
        }
    }

    fn demo_capability(id: &str) -> DemoCapability {
        let tool = |name: &str| ToolSpec {
            name: name.into(),
            description: "demo tool".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::ReadOnly,
        };
        DemoCapability {
            manifest: CapabilityManifest {
                id: id.into(),
                version: "0.1.0".into(),
                name: id.into(),
                summary: "demo".into(),
                status: CapabilityStatus::Experimental,
                provides: Vec::new(),
                permissions: Vec::new(),
                requires: Vec::new(),
                tools: vec![
                    tool(&format!("{id}.one")),
                    tool(&format!("{id}.two")),
                    tool(&format!("{id}.three")),
                ],
                lifecycle: CapabilityLifecycle::Lazy,
                transport: CapabilityTransport::Builtin,
            },
        }
    }

    #[test]
    fn loading_one_capability_tool_never_surfaces_siblings() {
        let registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(demo_capability("demo")))
            .expect("registration succeeds");

        // Registration alone leaves everything off the surface.
        assert!(registry.loaded_tool_specs().is_empty());

        registry.load_tool("demo.one").expect("load one tool");
        let surfaced: Vec<String> = registry
            .loaded_tool_specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert_eq!(surfaced, vec!["demo.one"]);

        // The sibling stays Available; the loaded tool is Loaded.
        assert_eq!(registry.tool_state("demo.one"), Some(ToolLifecycle::Loaded));
        assert_eq!(
            registry.tool_state("demo.two"),
            Some(ToolLifecycle::Available)
        );
        assert_eq!(
            registry.tool_state("demo.three"),
            Some(ToolLifecycle::Available)
        );

        // Discovery rows agree with the per-tool surface.
        let rows = registry.catalog_rows();
        let by_name = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .unwrap_or_else(|| panic!("{name} must be listed"))
        };
        assert_eq!(by_name("demo.one").state, ToolLifecycle::Loaded);
        assert_eq!(by_name("demo.two").state, ToolLifecycle::Available);
        assert_eq!(by_name("demo.three").state, ToolLifecycle::Available);
    }

    #[test]
    fn unloading_one_capability_tool_keeps_siblings_loaded() {
        let registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(demo_capability("demo")))
            .expect("registration succeeds");
        registry.load_tool("demo.one").expect("load one");
        registry.load_tool("demo.two").expect("load two");

        registry.unload_tool("demo.one").expect("unload one");
        let surfaced: Vec<String> = registry
            .loaded_tool_specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert_eq!(surfaced, vec!["demo.two"]);
        assert_eq!(
            registry.tool_state("demo.one"),
            Some(ToolLifecycle::Available)
        );
        assert_eq!(registry.tool_state("demo.two"), Some(ToolLifecycle::Loaded));
    }

    #[test]
    fn capability_snapshot_restore_keeps_per_tool_surface() {
        let registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(demo_capability("demo")))
            .expect("registration succeeds");
        registry.load_tool("demo.two").expect("load two");

        let snapshot = registry.snapshot();
        let restored = CapabilityRegistry::new();
        restored
            .register(Arc::new(demo_capability("demo")))
            .expect("registration succeeds");
        restored.restore(&snapshot);

        let surfaced: Vec<String> = restored
            .loaded_tool_specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert_eq!(surfaced, vec!["demo.two"]);
        // The snapshot wrote the authoritative per-tool list.
        assert_eq!(snapshot[0].loaded_tools, vec!["demo.two".to_string()]);
        assert!(snapshot[0].loaded);
    }

    #[test]
    fn legacy_whole_capability_checkpoint_migrates_to_all_tools() {
        let registry = CapabilityRegistry::new();
        registry
            .register(Arc::new(demo_capability("demo")))
            .expect("registration succeeds");
        // Old checkpoints carry `loaded: true` and no per-tool list; restore
        // must migrate them to "every declared tool loaded".
        registry.restore(&[crate::checkpoint::CapabilitySnapshot {
            id: "demo".into(),
            activation: CapabilityActivation::Enabled,
            loaded: true,
            loaded_tools: Vec::new(),
        }]);
        let mut surfaced: Vec<String> = registry
            .loaded_tool_specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        surfaced.sort();
        assert_eq!(surfaced, vec!["demo.one", "demo.three", "demo.two"]);
    }

    #[test]
    fn legacy_capability_snapshot_json_without_tool_list_deserializes() {
        // Old journal/checkpoint JSON has no loaded_tools field; it must
        // deserialize without fabricating a per-tool claim.
        let json = serde_json::json!({
            "id": "demo",
            "activation": "enabled",
            "loaded": true
        });
        let snapshot: crate::checkpoint::CapabilitySnapshot = serde_json::from_value(json).unwrap();
        assert!(snapshot.loaded);
        assert!(snapshot.loaded_tools.is_empty());
    }
}

#[async_trait]
impl ToolDispatcher for CapabilityAwareDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        // Keep the compatibility read just as coherent as the round path;
        // `snapshot` no longer delegates back to `specs`, so this cannot
        // recurse.
        self.snapshot().specs
    }

    fn snapshot(&self) -> ToolSurfaceSnapshot {
        // Return the complete currently-loaded candidate surface. The
        // RuntimeActor's RoundSurfacePlan owns the only schema-budget
        // projection because Task Must/Prefer/KeepReady demands must be able
        // to participate before anything is omitted.
        // Freeze capability surface writers while the independent base
        // dispatcher captures its own atomic snapshot. `inner` is not held
        // across that callback, and the base layer cannot depend on this
        // runtime registry, so the two source revisions describe one real
        // common cut without retries or an unstable fallback.
        let _surface = self
            .capabilities
            .surface_gate
            .read()
            .expect("capability registry poisoned");
        let (capability_specs, capability_generation) = self.capabilities.loaded_surface();
        let base_snapshot = self.base.snapshot();
        let base_generation = base_snapshot.generation;
        let mut source_revisions = base_snapshot.source_revisions;
        source_revisions.capability_catalog_generation = capability_generation;
        let mut specs: Vec<ToolSpec> = base_snapshot
            .specs
            .into_iter()
            // The unified control tool lives here; drop the builtin's own
            // copy so the surface has no duplicate.
            .filter(|spec| spec.name != CAPABILITY_MANAGE)
            .collect();
        specs.extend(capability_specs);
        specs.extend(Self::control_specs());
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        ToolSurfaceSnapshot {
            specs,
            // The unified surface generation: the base catalog's generation
            // (its own load/unload/gc transitions) combined with the
            // capability registry's (register/activation/load/unload), so a
            // dynamic capability change is auditably visible in the
            // snapshot — the generation tracks the whole surface, not just
            // the builtin half of it.
            generation: base_generation.wrapping_add(capability_generation),
            source_revisions,
            ..ToolSurfaceSnapshot::default()
        }
    }

    fn may_omit_from_round(&self, name: &str) -> bool {
        // A capability tool is optional at this pre-TaskAnchor stage. Base
        // tools delegate to their own catalog because only the concrete
        // provider knows which entries are configured as core. Runtime
        // controls remain fail-closed in either path.
        if matches!(name, CAPABILITY_MANAGE | agent_contracts::CONTEXT_MANAGE) {
            false
        } else if self.capabilities.owner_of(name).is_some() {
            true
        } else {
            self.base.may_omit_from_round(name)
        }
    }

    fn gc(&self) {
        self.base.gc();
    }

    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        self.unified_catalog()
    }

    fn load_tool(&self, name: &str) -> AgentResult<()> {
        if self.capabilities.owner_of(name).is_some() {
            return self.capabilities.load_tool(name);
        }
        self.base.load_tool(name)
    }

    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        if self.capabilities.owner_of(name).is_some() {
            return self.capabilities.unload_tool(name);
        }
        self.base.unload_tool(name)
    }

    fn inspect_tool(&self, name: &str) -> Option<ToolSpec> {
        self.capabilities
            .tool_spec(name)
            .or_else(|| self.base.inspect_tool(name))
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_MANAGE => self.run_manage(request).await.map(ToolOutcome::Value),
            _ => {
                if self.capabilities.owner_of(&name).is_some() {
                    let capability = self
                        .capabilities
                        .by_tool(&name)
                        .ok_or_else(|| AgentError::Tool(format!("unknown tool: {name}")))?;
                    let id = capability.manifest().id.clone();
                    // Activation gate (defense in depth: `load_tool` already
                    // blocks disabled capabilities; the route here must too).
                    match self.capabilities.activation(&id) {
                        Some(activation) if activation.usable() => {}
                        Some(activation) => {
                            return Err(AgentError::Tool(format!(
                                "capability '{id}' is {}; enable it before invoking its tools",
                                activation.as_str()
                            )));
                        }
                        None => return Err(AgentError::Tool(format!("unknown tool: {name}"))),
                    }
                    self.capabilities.ensure_started(&id).await?;
                    self.capabilities.mark_active(&name);
                    let ctx = self.invocation_context(&capability, &request);
                    let outcome = capability.invoke(request.call, ctx).await;
                    self.capabilities.mark_idle(&name);
                    // The core owns every side effect: a capability can only
                    // stage an effect (the actor commits it behind the
                    // generation fence) or attach a runtime directive —
                    // which is refused unless the manifest declares
                    // `runtime:context-control`. A plain value passes
                    // through unchanged.
                    return match outcome? {
                        CapabilityOutcome::Value(output) => Ok(ToolOutcome::Value(output)),
                        CapabilityOutcome::EffectRequest { output, effect } => {
                            Ok(ToolOutcome::PreparedEffect { output, effect })
                        }
                        CapabilityOutcome::RuntimeDirective { output, directive } => {
                            let manifest = capability.manifest();
                            if manifest
                                .permissions
                                .iter()
                                .any(|permission| permission == RUNTIME_CONTEXT_CONTROL)
                            {
                                Ok(ToolOutcome::RuntimeDirective { output, directive })
                            } else {
                                Ok(ToolOutcome::Value(ToolOutput {
                                    ok: false,
                                    summary: format!(
                                        "capability '{}' attempted a runtime directive without '{}' permission",
                                        manifest.id, RUNTIME_CONTEXT_CONTROL
                                    ),
                                    model_content: format!(
                                        "runtime directive denied: capability '{}' does not hold '{}' permission",
                                        manifest.id, RUNTIME_CONTEXT_CONTROL
                                    ),
                                    ..output
                                }))
                            }
                        }
                    };
                }
                self.base.execute(request).await
            }
        }
    }
}

/// A staged-only view over a confined workspace handle, handed to a
/// capability that declared `workspace:write`. The direct `write` path is
/// refused: a capability must stage a mutation as an `EffectRequest` via
/// `prepare_write`, and the runtime commits it behind the generation fence —
/// the capability computes, the core executes. A mutation applied during
/// `invoke` would bypass actor generation, cancellation and effect rollback.
struct StagedOnlyWorkspace(Arc<dyn WorkspaceHandle>);

#[async_trait]
impl WorkspaceHandle for StagedOnlyWorkspace {
    fn root(&self) -> &Path {
        self.0.root()
    }
    async fn resolve(&self, relative: &str) -> AgentResult<PathBuf> {
        self.0.resolve(relative).await
    }
    async fn read(&self, relative: &str) -> AgentResult<Vec<u8>> {
        self.0.read(relative).await
    }
    async fn write(&self, _relative: &str, _content: &[u8]) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(
            "capability writes must be staged: call prepare_write and return a CapabilityOutcome::EffectRequest; direct write bypasses the effect fence".into(),
        ))
    }
    async fn prepare_write(&self, relative: &str, content: &[u8]) -> AgentResult<Box<dyn Effect>> {
        self.0.prepare_write(relative, content).await
    }
}

/// A read-only view over a confined workspace handle, handed to a
/// capability that declared `workspace:read` but not `workspace:write`.
/// The write path is blocked by construction, not by trust.
struct ReadOnlyWorkspace(Arc<dyn WorkspaceHandle>);

#[async_trait]
impl WorkspaceHandle for ReadOnlyWorkspace {
    fn root(&self) -> &Path {
        self.0.root()
    }
    async fn resolve(&self, relative: &str) -> AgentResult<PathBuf> {
        self.0.resolve(relative).await
    }
    async fn read(&self, relative: &str) -> AgentResult<Vec<u8>> {
        self.0.read(relative).await
    }
    async fn write(&self, _relative: &str, _content: &[u8]) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(
            "workspace:write was not granted to this capability".into(),
        ))
    }
    async fn prepare_write(
        &self,
        _relative: &str,
        _content: &[u8],
    ) -> AgentResult<Box<dyn Effect>> {
        Err(AgentError::InvalidRequest(
            "workspace:write was not granted to this capability".into(),
        ))
    }
}

impl CapabilityAwareDispatcher {
    /// Build the invocation context for one capability call: the manifest's
    /// declared permissions plus confined handles for everything the
    /// runtime actually wired in. A capability that declared no workspace
    /// permission gets no workspace handle at all.
    fn invocation_context(
        &self,
        capability: &Arc<dyn Capability>,
        request: &ToolExecutionRequest,
    ) -> CapabilityInvocationContext {
        let manifest = capability.manifest();
        let permissions = manifest.permissions.clone();
        let declares = |permission: &str| permissions.iter().any(|p| p == permission);

        let workspace = if declares(WORKSPACE_READ) || declares(WORKSPACE_WRITE) {
            self.workspace.as_ref().map(|workspace| {
                let handle: Arc<dyn WorkspaceHandle> =
                    Arc::new(ConfinedWorkspaceHandle::new(workspace, &request.call.name));
                if declares(WORKSPACE_WRITE) {
                    // Writes are staged, never applied during invoke.
                    Arc::new(StagedOnlyWorkspace(handle)) as Arc<dyn WorkspaceHandle>
                } else {
                    Arc::new(ReadOnlyWorkspace(handle)) as Arc<dyn WorkspaceHandle>
                }
            })
        } else {
            None
        };
        let artifacts = if permissions.iter().any(|p| p.starts_with("artifact")) {
            self.workspace.as_ref().map(|workspace| {
                let handle: Arc<dyn ArtifactHandle> =
                    Arc::new(ArtifactStoreHandle::new(workspace, request.run_id));
                handle
            })
        } else {
            None
        };
        CapabilityInvocationContext {
            granted_permissions: permissions,
            workspace,
            artifacts,
            cancel: request.cancel.clone(),
        }
    }
}

#[derive(serde::Deserialize)]
struct ManageArgs {
    op: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    /// Max rows in the model-facing page (default 20, capped at 50).
    #[serde(default)]
    limit: Option<usize>,
    /// Opaque cursor for paging: the last tool name of the previous page.
    #[serde(default)]
    cursor: Option<String>,
}

impl CapabilityAwareDispatcher {
    async fn run_search(
        &self,
        request: ToolExecutionRequest,
        args: SearchArgs,
    ) -> AgentResult<ToolOutput> {
        let page_size = args
            .limit
            .unwrap_or(CAPABILITY_SEARCH_DEFAULT_LIMIT)
            .clamp(1, CAPABILITY_SEARCH_MAX_LIMIT);
        let mut entries = self.unified_catalog();
        if let Some(query) = args.query.as_deref() {
            entries.retain(|entry| entry.name.contains(query));
        }
        let active = entries
            .iter()
            .filter(|entry| entry.state.in_surface())
            .count();
        let total = entries.len();
        // A catalog that does not fit the page spills its full listing to
        // an artifact — the model only ever sees the bounded page, so a
        // 1000-capability catalog cannot itself become context pollution.
        let artifact_ref = if total > page_size {
            match &self.workspace {
                Some(workspace) => {
                    let all: String = entries
                        .iter()
                        .map(|entry| {
                            format!(
                                "{}\t{}\t[{}]",
                                entry.state.as_str(),
                                entry.name,
                                entry.owner
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some(
                        workspace
                            .write_artifact(
                                request.run_id,
                                "capability-search",
                                "txt",
                                all.as_bytes(),
                            )
                            .await?,
                    )
                }
                None => None,
            }
        } else {
            None
        };
        if let Some(cursor) = args.cursor.as_deref() {
            entries.retain(|entry| entry.name.as_str() > cursor);
        }
        let remaining = entries.len();
        let page: Vec<_> = entries.into_iter().take(page_size).collect();
        let has_more = remaining > page.len();
        let lines: Vec<String> = page
            .iter()
            .map(|entry| {
                format!(
                    "{}\t{}\t[{}]",
                    entry.state.as_str(),
                    entry.name,
                    entry.owner
                )
            })
            .collect();
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("{} tools matched ({} on the model surface)", total, active),
            model_content: if lines.is_empty() {
                "no tools match".to_string()
            } else {
                lines.join("\n")
            },
            artifact_ref,
            metadata: json!({
                "total": total,
                "active": active,
                "returned": page.len(),
                "has_more": has_more,
            }),
        })
    }

    async fn run_inspect(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        let Some(spec) = self.inspect_tool(&name) else {
            return Ok(ToolOutput {
                call_id: request.call.id,
                tool_name: CAPABILITY_MANAGE.into(),
                ok: false,
                summary: format!("unknown tool: {name}"),
                model_content: format!("unknown tool: {name}"),
                artifact_ref: None,
                metadata: json!({}),
            });
        };
        let state = self
            .capabilities
            .tool_state(&name)
            .or_else(|| {
                self.base
                    .catalog()
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.state)
            })
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let owner = self
            .capabilities
            .owner_of(&name)
            .or_else(|| {
                self.base
                    .catalog()
                    .into_iter()
                    .find(|entry| entry.name == name)
                    .map(|entry| entry.owner)
            })
            .unwrap_or_else(|| "builtin".to_string());
        let activation = self
            .capabilities
            .owner_of(&name)
            .and_then(|owner_id| self.capabilities.activation(&owner_id))
            .map(|activation| activation.as_str().to_string())
            .unwrap_or_else(|| "n/a".to_string());
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool {name}: {state}"),
            model_content: format!(
                "name: {}\nowner: {}\nactivation: {}\nstate: {}\ndescription: {}\nschema: {}",
                spec.name, owner, activation, state, spec.description, spec.input_schema
            ),
            artifact_ref: None,
            metadata: json!({
                "name": spec.name,
                "owner": owner,
                "activation": activation,
                "state": state,
                "risk": format!("{:?}", spec.risk),
            }),
        })
    }

    async fn run_load(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        self.load_tool(&name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool loaded: {name}"),
            model_content: format!("tool loaded: {name} — its schema is now offered to the model"),
            artifact_ref: None,
            metadata: json!({"tool": name}),
        })
    }

    async fn run_unload(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        self.unload_tool(&name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool unloaded: {name}"),
            model_content: format!("tool unloaded: {name}"),
            artifact_ref: None,
            metadata: json!({"tool": name}),
        })
    }

    /// Dispatch `capability.manage` ops: search / inspect / load / unload.
    async fn run_manage(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: ManageArgs = serde_json::from_value(request.call.arguments.clone())
            .map_err(|e| AgentError::InvalidRequest(format!("capability.manage args: {e}")))?;
        match args.op.as_str() {
            "search" => {
                let search = SearchArgs {
                    query: args.query,
                    limit: args.limit,
                    cursor: args.cursor,
                };
                self.run_search(request, search).await
            }
            "inspect" => {
                let name = args.name.ok_or_else(|| {
                    AgentError::InvalidRequest("capability.manage inspect: missing 'name'".into())
                })?;
                self.run_inspect(request, name).await
            }
            "load" => {
                let name = args.name.ok_or_else(|| {
                    AgentError::InvalidRequest("capability.manage load: missing 'name'".into())
                })?;
                self.run_load(request, name).await
            }
            "unload" => {
                let name = args.name.ok_or_else(|| {
                    AgentError::InvalidRequest("capability.manage unload: missing 'name'".into())
                })?;
                self.run_unload(request, name).await
            }
            other => Err(AgentError::InvalidRequest(format!(
                "capability.manage: unknown op '{other}' (expected search/inspect/load/unload)"
            ))),
        }
    }
}

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
    sync::{Arc, RwLock},
};

use agent_contracts::{
    AgentError, AgentResult, ArtifactHandle, CAPABILITY_INSPECT, CAPABILITY_LOAD,
    CAPABILITY_SEARCH, CAPABILITY_UNLOAD, Capability, CapabilityActivation,
    CapabilityInvocationContext, CapabilityLifecycle, CapabilityOutcome, CapabilityStatus,
    CapabilityTransport, Effect, RUNTIME_CONTEXT_CONTROL, ToolCatalogEntry, ToolDispatcher,
    ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolOutput, ToolSpec, ToolSurfaceSnapshot,
    WorkspaceHandle,
};
use agent_workspace::{ArtifactStoreHandle, ConfinedWorkspaceHandle, Workspace};
use async_trait::async_trait;
use serde_json::json;

struct Entry {
    capability: Arc<dyn Capability>,
    started: bool,
    /// The effective maturity. External (out-of-process) capabilities are
    /// pinned to Experimental regardless of their declared status, so an LLM
    /// cannot promote its own module to Stable.
    status: CapabilityStatus,
    /// Whether the runtime will run this capability at all. External
    /// capabilities enter `Disabled`; only an explicit enable (operator or
    /// evaluator) makes them usable.
    activation: CapabilityActivation,
    /// Whether the capability's tools are on the model surface. Registration
    /// alone keeps them `Available`; `capability.load` (or the runtime) puts
    /// them on the surface, `capability.unload` takes them off.
    loaded: bool,
    /// A tool of this capability is executing right now.
    active: bool,
}

/// One row of the platform's capability catalog (the discovery surface).
#[derive(Debug, Clone)]
pub struct CapabilityCatalogEntry {
    pub id: String,
    pub status: CapabilityStatus,
    pub activation: CapabilityActivation,
    pub transport: CapabilityTransport,
    pub tools: Vec<String>,
}

/// Runtime-mutable registry of dynamic capabilities, shared between the
/// module host (registration) and the tool dispatcher (specs + routing).
/// Registration is not gated on the host lifecycle: a capability can be
/// published mid-run and its tools appear on the next model request.
#[derive(Default)]
pub struct CapabilityRegistry {
    inner: RwLock<HashMap<String, Entry>>,
    /// Tool names the runtime owns (builtin core tools plus the unified
    /// control tools). A capability may never shadow them: routing would
    /// otherwise be hijackable by declaration.
    reserved: RwLock<HashSet<String>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
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
    /// never see a half-wired tool or an ambiguous route.
    pub fn register(&self, capability: Arc<dyn Capability>) -> AgentResult<()> {
        let manifest = capability.manifest();
        let tool_specs = capability.tool_specs();
        let tool_names: Vec<&str> = tool_specs.iter().map(|spec| spec.name.as_str()).collect();

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
                    .capability
                    .tool_specs()
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
                started: false,
                status,
                activation,
                loaded: false,
                active: false,
            },
        );
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
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let entry = inner.get_mut(id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
        })?;
        entry.activation = activation;
        // A capability that cannot run must not keep its tools on the
        // model surface.
        if !activation.usable() {
            entry.loaded = false;
        }
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
                let manifest = entry.capability.manifest();
                CapabilityCatalogEntry {
                    id: manifest.id.clone(),
                    status: entry.status,
                    activation: entry.activation,
                    transport: manifest.transport.clone(),
                    tools: entry
                        .capability
                        .tool_specs()
                        .iter()
                        .map(|s| s.name.clone())
                        .collect(),
                }
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// The tool schemas of *loaded and usable* capabilities only — the
    /// model surface. Unloaded or disabled capabilities stay registered but
    /// invisible, so the prompt does not grow with every registered
    /// capability and a suspended one cannot linger on the surface.
    pub fn loaded_tool_specs(&self) -> Vec<ToolSpec> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner
            .values()
            .filter(|entry| entry.loaded && entry.activation.usable())
            .flat_map(|entry| entry.capability.tool_specs())
            .collect()
    }

    /// The capability that owns a tool name, if any.
    pub fn by_tool(&self, tool_name: &str) -> Option<Arc<dyn Capability>> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .capability
                .tool_specs()
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
                .capability
                .tool_specs()
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| id.clone())
        })
    }

    /// Unified `capability.load`: put every tool of the owning capability on
    /// the model surface. Unknown tool names are rejected like the builtin
    /// catalog does, and a disabled/quarantined capability cannot load —
    /// activation is the gate in front of the surface.
    pub fn load_tool(&self, tool_name: &str) -> AgentResult<()> {
        let owner = self
            .owner_of(tool_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
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
        entry.loaded = true;
        Ok(())
    }

    /// Unified `capability.unload`: take the owning capability's tools off
    /// the model surface.
    pub fn unload_tool(&self, tool_name: &str) -> AgentResult<()> {
        let owner = self
            .owner_of(tool_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        let mut inner = self.inner.write().expect("capability registry poisoned");
        if let Some(entry) = inner.get_mut(&owner) {
            entry.loaded = false;
        }
        Ok(())
    }

    /// Lifecycle state of one capability tool: `Active` while executing,
    /// `Loaded` when its capability is loaded, `Available` otherwise. A
    /// disabled/quarantined capability reports `Available` regardless —
    /// its tools are not on the surface.
    pub fn tool_state(&self, tool_name: &str) -> Option<ToolLifecycle> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .capability
                .tool_specs()
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| {
                    if entry.active {
                        ToolLifecycle::Active
                    } else if entry.loaded && entry.activation.usable() {
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
                .capability
                .tool_specs()
                .iter()
                .find(|spec| spec.name == tool_name)
                .cloned()
        })
    }

    /// Unified discovery rows: every capability tool with its owner id and
    /// lifecycle state.
    pub fn catalog_rows(&self) -> Vec<ToolCatalogEntry> {
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut rows = Vec::new();
        for (id, entry) in inner.iter() {
            let owner = id.clone();
            let active = entry.active;
            let usable = entry.loaded && entry.activation.usable();
            for spec in entry.capability.tool_specs() {
                let state = if active {
                    ToolLifecycle::Active
                } else if usable {
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
    }

    /// Snapshot of every registered capability's surface state (activation +
    /// loaded), for checkpoints. Registration identity itself is not part of
    /// the snapshot: capabilities are re-registered by the composition root
    /// on a fresh run, then this re-applies their flags.
    pub fn snapshot(&self) -> Vec<crate::checkpoint::CapabilitySnapshot> {
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .iter()
            .map(|(id, entry)| crate::checkpoint::CapabilitySnapshot {
                id: id.clone(),
                activation: entry.activation,
                loaded: entry.loaded,
            })
            .collect();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries
    }

    /// Re-apply a checkpoint's capability surface state: activation first,
    /// then the loaded flag (a loaded flag without a usable activation is
    /// dropped — activation is the gate in front of the model surface).
    pub fn restore(&self, state: &[crate::checkpoint::CapabilitySnapshot]) {
        for entry in state {
            let mut inner = self.inner.write().expect("poisoned");
            let Some(current) = inner.get_mut(&entry.id) else {
                // The capability is not registered in this run; its flags
                // have nothing to apply to.
                continue;
            };
            current.activation = entry.activation;
            current.loaded = entry.loaded && entry.activation.usable();
        }
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
                        && entry.capability.manifest().lifecycle == CapabilityLifecycle::Eager
                        && !entry.started
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.ensure_started(&id).await?;
        }
        Ok(())
    }

    /// Start a capability on first use (lazy lifecycle) and mark it started.
    /// A disabled/quarantined capability is rejected: the start is the point
    /// where "not usable" would otherwise turn into a running process.
    pub async fn ensure_started(&self, id: &str) -> AgentResult<()> {
        let already_started = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .is_some_and(|entry| entry.started);
        if already_started {
            return Ok(());
        }
        let capability = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .get(id)
            .map(|entry| {
                if !entry.activation.usable() {
                    return Err(AgentError::InvalidRequest(format!(
                        "capability '{id}' is {}; enable it before use",
                        entry.activation.as_str()
                    )));
                }
                Ok(entry.capability.clone())
            })
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
            })??;
        capability.start().await?;
        if let Some(entry) = self
            .inner
            .write()
            .expect("capability registry poisoned")
            .get_mut(id)
        {
            entry.started = true;
        }
        Ok(())
    }

    /// Stop every capability and reset the started flags (host stop).
    /// Best effort: every capability gets its stop call even when an
    /// earlier one fails, and all errors are aggregated into one result.
    pub async fn stop_all(&self) -> AgentResult<()> {
        let capabilities: Vec<Arc<dyn Capability>> = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .values()
            .map(|entry| entry.capability.clone())
            .collect();
        let mut errors = Vec::new();
        for capability in capabilities {
            if let Err(error) = capability.stop().await {
                errors.push(error);
            }
        }
        for entry in self
            .inner
            .write()
            .expect("capability registry poisoned")
            .values_mut()
        {
            entry.started = false;
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
        reserved.extend([
            CAPABILITY_SEARCH.to_string(),
            CAPABILITY_INSPECT.to_string(),
            CAPABILITY_LOAD.to_string(),
            CAPABILITY_UNLOAD.to_string(),
        ]);
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
                name: CAPABILITY_SEARCH.into(),
                description:
                    "List every known tool (builtin and dynamic capability) with its lifecycle state and owner."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Optional name filter"}
                    }
                }),
                risk: agent_contracts::ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: CAPABILITY_INSPECT.into(),
                description:
                    "Inspect one tool: its schema, risk class, owner and lifecycle state."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}}
                }),
                risk: agent_contracts::ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: CAPABILITY_LOAD.into(),
                description:
                    "Load a tool (or the capability owning it) into the active set so its schema appears in the model's tool schemas."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}}
                }),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            },
            ToolSpec {
                name: CAPABILITY_UNLOAD.into(),
                description:
                    "Unload a tool (or the capability owning it) from the active set; its schema stops being offered. Core builtin tools cannot be unloaded."
                        .into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}}
                }),
                risk: agent_contracts::ToolRisk::WorkspaceWrite,
            },
        ]
    }

    /// The unified catalog: builtin rows plus capability rows, sorted by name.
    fn unified_catalog(&self) -> Vec<ToolCatalogEntry> {
        let mut rows = self.base.catalog();
        rows.extend(self.capabilities.catalog_rows());
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }
}

#[async_trait]
impl ToolDispatcher for CapabilityAwareDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .base
            .specs()
            .into_iter()
            // The unified control tools live here; drop the builtin's own
            // copies so the surface has no duplicates.
            .filter(|spec| {
                !matches!(
                    spec.name.as_str(),
                    CAPABILITY_SEARCH | CAPABILITY_LOAD | CAPABILITY_UNLOAD
                )
            })
            .collect();
        specs.extend(self.capabilities.loaded_tool_specs());
        specs.extend(Self::control_specs());
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    fn snapshot(&self) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            specs: self.specs(),
            generation: self.base.snapshot().generation,
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
            CAPABILITY_SEARCH => self.run_search(request).await.map(ToolOutcome::Value),
            CAPABILITY_INSPECT => self.run_inspect(request).await.map(ToolOutcome::Value),
            CAPABILITY_LOAD => self.run_load(request).await.map(ToolOutcome::Value),
            CAPABILITY_UNLOAD => self.run_unload(request).await.map(ToolOutcome::Value),
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

        let workspace = if declares("workspace:read") || declares("workspace:write") {
            self.workspace.as_ref().map(|workspace| {
                let handle: Arc<dyn WorkspaceHandle> =
                    Arc::new(ConfinedWorkspaceHandle::new(workspace, &request.call.name));
                if declares("workspace:write") {
                    handle
                } else {
                    Arc::new(ReadOnlyWorkspace(handle))
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
struct NameArgs {
    name: String,
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
}

impl CapabilityAwareDispatcher {
    async fn run_search(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: SearchArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.search args: {e}")))?;
        let mut entries = self.unified_catalog();
        if let Some(query) = args.query.as_deref() {
            entries.retain(|entry| entry.name.contains(query));
        }
        let active = entries
            .iter()
            .filter(|entry| entry.state.in_surface())
            .count();
        let lines: Vec<String> = entries
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
            tool_name: CAPABILITY_SEARCH.into(),
            ok: true,
            summary: format!(
                "{} tools matched ({} on the model surface)",
                entries.len(),
                active
            ),
            model_content: lines.join("\n"),
            artifact_ref: None,
            metadata: json!({"total": entries.len(), "active": active}),
        })
    }

    async fn run_inspect(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: NameArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.inspect args: {e}")))?;
        let Some(spec) = self.inspect_tool(&args.name) else {
            return Ok(ToolOutput {
                call_id: request.call.id,
                tool_name: CAPABILITY_INSPECT.into(),
                ok: false,
                summary: format!("unknown tool: {}", args.name),
                model_content: format!("unknown tool: {}", args.name),
                artifact_ref: None,
                metadata: json!({}),
            });
        };
        let state = self
            .capabilities
            .tool_state(&args.name)
            .or_else(|| {
                self.base
                    .catalog()
                    .into_iter()
                    .find(|entry| entry.name == args.name)
                    .map(|entry| entry.state)
            })
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let owner = self
            .capabilities
            .owner_of(&args.name)
            .or_else(|| {
                self.base
                    .catalog()
                    .into_iter()
                    .find(|entry| entry.name == args.name)
                    .map(|entry| entry.owner)
            })
            .unwrap_or_else(|| "builtin".to_string());
        let activation = self
            .capabilities
            .owner_of(&args.name)
            .and_then(|owner_id| self.capabilities.activation(&owner_id))
            .map(|activation| activation.as_str().to_string())
            .unwrap_or_else(|| "n/a".to_string());
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_INSPECT.into(),
            ok: true,
            summary: format!("tool {}: {}", args.name, state),
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

    async fn run_load(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: NameArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.load args: {e}")))?;
        self.load_tool(&args.name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_LOAD.into(),
            ok: true,
            summary: format!("tool loaded: {}", args.name),
            model_content: format!(
                "tool loaded: {} — its schema is now offered to the model",
                args.name
            ),
            artifact_ref: None,
            metadata: json!({"tool": args.name}),
        })
    }

    async fn run_unload(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: NameArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.unload args: {e}")))?;
        self.unload_tool(&args.name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_UNLOAD.into(),
            ok: true,
            summary: format!("tool unloaded: {}", args.name),
            model_content: format!("tool unloaded: {}", args.name),
            artifact_ref: None,
            metadata: json!({"tool": args.name}),
        })
    }
}

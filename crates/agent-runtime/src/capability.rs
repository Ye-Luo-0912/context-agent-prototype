//! The dynamic capability plane of the runtime: a shared registry that
//! accepts capabilities at composition time or at runtime, and a tool
//! dispatcher that merges the trusted core's tools with the capabilities'
//! tools under one unified lifecycle — `capability.search` / `inspect` /
//! `load` / `unload` cover both, and a capability's tools only enter the
//! model surface when they are loaded (explicitly or by the runtime), so
//! the prompt does not grow with every registered capability.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use agent_contracts::{
    AgentError, AgentResult, Capability, CapabilityLifecycle, CapabilityStatus,
    CapabilityTransport, ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolLifecycle,
    ToolOutput, ToolSpec, ToolSurfaceSnapshot, CAPABILITY_INSPECT, CAPABILITY_LOAD,
    CAPABILITY_SEARCH, CAPABILITY_UNLOAD,
};
use async_trait::async_trait;
use serde_json::json;

struct Entry {
    capability: Arc<dyn Capability>,
    started: bool,
    /// The effective maturity. External (out-of-process) capabilities are
    /// pinned to Experimental regardless of their declared status, so an LLM
    /// cannot promote its own module to Stable.
    status: CapabilityStatus,
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
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capability. Rejects duplicate ids and missing declared
    /// dependencies so the model never sees a half-wired tool.
    pub fn register(&self, capability: Arc<dyn Capability>) -> AgentResult<()> {
        let manifest = capability.manifest();
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
        // The maturity ladder is climbed, not declared: out-of-process
        // (external/LLM-authored) capabilities always start Experimental.
        let status = if manifest.transport != CapabilityTransport::Builtin
            && manifest.status != CapabilityStatus::Experimental
        {
            CapabilityStatus::Experimental
        } else {
            manifest.status
        };
        inner.insert(
            manifest.id.clone(),
            Entry {
                capability,
                started: false,
                status,
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

    /// The tool schemas of *loaded* capabilities only — the model surface.
    /// Unloaded capabilities stay registered but invisible, so the prompt
    /// does not grow with every registered capability.
    pub fn loaded_tool_specs(&self) -> Vec<ToolSpec> {
        let inner = self.inner.read().expect("capability registry poisoned");
        inner
            .values()
            .filter(|entry| entry.loaded)
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
    /// catalog does.
    pub fn load_tool(&self, tool_name: &str) -> AgentResult<()> {
        let owner = self
            .owner_of(tool_name)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        let mut inner = self.inner.write().expect("capability registry poisoned");
        if let Some(entry) = inner.get_mut(&owner) {
            entry.loaded = true;
        }
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
    /// `Loaded` when its capability is loaded, `Available` otherwise.
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
                    } else if entry.loaded {
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
            let loaded = entry.loaded;
            for spec in entry.capability.tool_specs() {
                let state = if active {
                    ToolLifecycle::Active
                } else if loaded {
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
        if let Some(owner) = owner {
            if let Some(entry) = self
                .inner
                .write()
                .expect("capability registry poisoned")
                .get_mut(&owner)
            {
                entry.active = true;
            }
        }
    }

    /// Clear the executing marker after a call finished.
    pub fn mark_idle(&self, tool_name: &str) {
        let owner = self.owner_of(tool_name);
        if let Some(owner) = owner {
            if let Some(entry) = self
                .inner
                .write()
                .expect("capability registry poisoned")
                .get_mut(&owner)
            {
                entry.active = false;
            }
        }
    }

    /// Start every eager capability that has not started yet (host start).
    pub async fn start_eager(&self) -> AgentResult<()> {
        let ids: Vec<String> = {
            let inner = self.inner.read().expect("capability registry poisoned");
            inner
                .iter()
                .filter(|(_, entry)| {
                    entry.capability.manifest().lifecycle == CapabilityLifecycle::Eager
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
            .map(|entry| entry.capability.clone())
            .ok_or_else(|| {
                AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
            })?;
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
    pub async fn stop_all(&self) -> AgentResult<()> {
        let capabilities: Vec<Arc<dyn Capability>> = self
            .inner
            .read()
            .expect("capability registry poisoned")
            .values()
            .map(|entry| entry.capability.clone())
            .collect();
        for capability in capabilities {
            capability.stop().await?;
        }
        for entry in self
            .inner
            .write()
            .expect("capability registry poisoned")
            .values_mut()
        {
            entry.started = false;
        }
        Ok(())
    }
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
}

impl CapabilityAwareDispatcher {
    pub fn new(base: Arc<dyn ToolDispatcher>, capabilities: Arc<CapabilityRegistry>) -> Self {
        Self { base, capabilities }
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

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_SEARCH => self.run_search(request).await,
            CAPABILITY_INSPECT => self.run_inspect(request).await,
            CAPABILITY_LOAD => self.run_load(request).await,
            CAPABILITY_UNLOAD => self.run_unload(request).await,
            _ => {
                if self.capabilities.owner_of(&name).is_some() {
                    let capability = self
                        .capabilities
                        .by_tool(&name)
                        .ok_or_else(|| AgentError::Tool(format!("unknown tool: {name}")))?;
                    self.capabilities
                        .ensure_started(&capability.manifest().id)
                        .await?;
                    self.capabilities.mark_active(&name);
                    let output = capability.invoke(request.call).await;
                    self.capabilities.mark_idle(&name);
                    return output;
                }
                self.base.execute(request).await
            }
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
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_INSPECT.into(),
            ok: true,
            summary: format!("tool {}: {}", args.name, state),
            model_content: format!(
                "name: {}\nowner: {}\nstate: {}\ndescription: {}\nschema: {}",
                spec.name, owner, state, spec.description, spec.input_schema
            ),
            artifact_ref: None,
            metadata: json!({
                "name": spec.name,
                "owner": owner,
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

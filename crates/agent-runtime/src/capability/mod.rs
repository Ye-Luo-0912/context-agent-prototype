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
    CapabilityTransport, Effect, RUNTIME_CONTEXT_CONTROL, HostToolPolicies, ResourceDescriptor,
    ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolLifecycle, ToolOutcome, ToolOutput,
    ToolSemanticRole, ToolSpec, ToolSurfaceSnapshot, WORKSPACE_READ, WORKSPACE_WRITE,
    WorkspaceHandle, compact_tool_purpose, search_tool_catalog_filtered, unbound_effect_intent,
};
use agent_core::{CapabilityAdmission, CapabilityState, CapabilityStateAuthority};
use agent_workspace::{ArtifactStoreHandle, ConfinedWorkspaceHandle, RemoteEffectAck, Workspace};
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
    /// Which tools of this capability are on the model surface and how
    /// recently each was used. This is per-tool surface state, split from
    /// the capability's process lifecycle (`run_state`): `capability.load`
    /// (or the runtime) puts exactly the named tool on the surface —
    /// sibling tools of the same capability stay off until they are loaded
    /// individually — and each loaded tool ages independently
    /// (Loaded → Warm → Unloaded) exactly like the builtin catalog, so a
    /// task that requires a capability tool roots it against idle GC the
    /// same way it roots a builtin.
    tool_states: HashMap<String, CapabilityToolState>,
    /// A tool of this capability is executing right now (per-capability
    /// executing marker; the catalog's `Active` row).
    active: bool,
}

/// Per-tool surface lifecycle of one capability tool: the tool's
/// model-surface state (Loaded / Warm / Unloaded; absent means Available)
/// and the tick of its last use, which idle GC ages against. The split is
/// deliberate — loading one tool never exposes its siblings, and each tool
/// cools independently of the capability's process lifecycle.
#[derive(Debug, Clone, Copy)]
struct CapabilityToolState {
    lifecycle: ToolLifecycle,
    last_used_tick: u64,
}

/// Fail-closed meet for activation restored from an older authority
/// snapshot. Restore may preserve or reduce live authority, never promote a
/// currently Disabled/Quarantined capability back to Enabled. Quarantine is
/// the strongest state and therefore also survives a stale Disabled row.
fn restore_activation_meet(
    current: CapabilityActivation,
    checkpoint: CapabilityActivation,
) -> CapabilityActivation {
    use CapabilityActivation::{Disabled, Enabled, Quarantined};
    match (current, checkpoint) {
        (Quarantined, _) | (_, Quarantined) => Quarantined,
        (Disabled, _) | (_, Disabled) => Disabled,
        (Enabled, Enabled) => Enabled,
    }
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
pub struct CapabilityRegistry {
    /// Coordinates a unified snapshot with surface mutations without holding
    /// `inner` across the lower dispatcher's `snapshot()` callback. Writers
    /// always acquire this gate before `inner`; readers may consequently
    /// freeze the capability half while the independent base snapshot is
    /// captured at one common linearization point.
    surface_gate: RwLock<()>,
    inner: RwLock<HashMap<String, Entry>>,
    /// The core-owned record of each registered capability's effective
    /// maturity and activation. Every state read and every transition
    /// (enable/disable/quarantine) routes through this authority — the
    /// registry keeps only the mutable surface mechanics (loaded tools,
    /// active marks, run lifecycle) that react to it.
    state: CapabilityStateAuthority,
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
    /// Monotonic lifecycle tick: bumped at every load and every gc() safe
    /// point, so per-tool idle aging measures rounds, like the builtin
    /// catalog.
    tick: AtomicU64,
    /// Idle model rounds before a loaded capability tool cools to Warm.
    idle_to_warm_ticks: u64,
    /// Idle model rounds before a warm capability tool is unloaded from the model
    /// surface.
    warm_to_unload_ticks: u64,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self {
            surface_gate: RwLock::new(()),
            inner: RwLock::new(HashMap::new()),
            state: CapabilityStateAuthority::default(),
            reserved: RwLock::new(HashSet::new()),
            generation: AtomicU64::new(0),
            catalog_version: AtomicU64::new(0),
            rows_cache: RwLock::new(None),
            tick: AtomicU64::new(0),
            // Defaults mirror the builtin catalog's idle thresholds, so
            // capability tools and builtin tools cool at the same rate.
            idle_to_warm_ticks: 8,
            warm_to_unload_ticks: 24,
        }
    }
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with explicit idle thresholds for the per-tool surface
    /// lifecycle (Loaded → Warm → Unloaded). The defaults mirror the
    /// builtin catalog; tests and composition roots can tune them.
    pub fn with_idle_thresholds(idle_to_warm_ticks: u64, warm_to_unload_ticks: u64) -> Self {
        Self {
            idle_to_warm_ticks,
            warm_to_unload_ticks,
            ..Self::default()
        }
    }

    /// Current lifecycle-clock value. Loads and active-use stamps carry
    /// it; ONLY `gc` — the merged dispatcher's once-per-model-round safe
    /// point — advances the clock, so idle thresholds mean "model rounds
    /// without use". A load or a tool call must never make time pass
    /// faster (that feedback loop cooled in-use tools mid-task).
    fn stamp_now(&self) -> u64 {
        self.tick.load(Ordering::Relaxed)
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
        // Admission is a core decision: the core's stateless authority
        // validates the manifest, tool schemas and authority derivation
        // lock-free, then checks collisions against this registry's live
        // state (duplicate id, missing requires, shadowed/owned tool names).
        CapabilityAdmission::validate_static(&manifest, &tool_specs)?;
        let tool_names: Vec<&str> = tool_specs.iter().map(|spec| spec.name.as_str()).collect();

        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let reserved = self.reserved.read().expect("capability registry poisoned");
        let ctx = agent_core::AdmissionContext {
            is_registered: &|id| inner.contains_key(id),
            reserved_names: &reserved,
            owner_of_tool: &|name| {
                inner.iter().find_map(|(id, entry)| {
                    entry
                        .tool_specs
                        .iter()
                        .any(|spec| spec.name == name)
                        .then(|| id.clone())
                })
            },
        };
        CapabilityAdmission::validate_collisions(&manifest, &tool_names, &ctx)?;
        drop(reserved);

        // Maturity and activation are decided by the core's admission
        // authority, never declared: external capabilities always start
        // Experimental and Disabled; only the trusted in-process core keeps
        // its declared status and starts Enabled.
        let status = CapabilityAdmission::initial_status(&manifest);
        let activation = CapabilityAdmission::initial_activation(&manifest);
        // Core owns the state: record maturity + activation and the
        // effective permission grant in the authority before the entry
        // appears in the surface maps, so no reader ever sees an entry
        // without its state record.
        self.state.register(
            &manifest.id,
            CapabilityState { status, activation },
            manifest.permissions.clone(),
        )?;
        inner.insert(
            manifest.id.clone(),
            Entry {
                capability,
                manifest,
                tool_specs,
                run_state: CapabilityRunState::Stopped,
                run_lock: Arc::new(tokio::sync::Mutex::new(())),
                tool_states: HashMap::new(),
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
    /// external capabilities regardless of declaration). The record lives
    /// in the core's capability-state authority.
    pub fn status(&self, id: &str) -> Option<CapabilityStatus> {
        self.state.status(id)
    }

    /// The current activation of a registered capability. The record lives
    /// in the core's capability-state authority.
    pub fn activation(&self, id: &str) -> Option<CapabilityActivation> {
        self.state.activation(id)
    }

    /// The effective permission grant of a registered capability — what the
    /// runtime may hand it at invoke time. The grant is a core record
    /// captured at registration, so a capability that returns a different
    /// manifest after registration cannot escalate what it holds.
    pub fn granted_permissions(&self, id: &str) -> Option<Arc<Vec<String>>> {
        self.state.granted_permissions(id)
    }

    /// Set a capability's activation: `Enabled` makes it loadable and
    /// invocable, `Disabled`/`Quarantined` take its tools off the model
    /// surface and block further calls. Enabling is the operator/evaluator
    /// action that external capabilities wait for. The transition is
    /// decided and recorded by the core's capability-state authority; the
    /// registry reacts to it with the surface effects (loaded tools,
    /// generation bumps).
    pub fn set_activation(&self, id: &str, activation: CapabilityActivation) -> AgentResult<()> {
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        // The entry must exist for the surface effects below; the authority
        // record is updated right after, so the two never diverge.
        let entry = inner.get_mut(id).ok_or_else(|| {
            AgentError::InvalidRequest(format!("capability '{id}' is not registered"))
        })?;
        self.state.set_activation(id, activation)?;
        // A capability that cannot run must not keep its tools on the
        // model surface.
        if !activation.usable() {
            entry.tool_states.clear();
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
        // Pre-fetch the core state record before taking `inner`, so this
        // read never nests the authority lock inside the registry lock.
        let states = self.state.state_map();
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .values()
            .filter_map(|entry| {
                let manifest = &entry.manifest;
                let state = states.get(&manifest.id).copied()?;
                Some(CapabilityCatalogEntry {
                    id: manifest.id.clone(),
                    status: state.status,
                    activation: state.activation,
                    transport: manifest.transport.clone(),
                    tools: entry.tool_specs.iter().map(|s| s.name.clone()).collect(),
                    run_state: entry.run_state,
                })
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
        // Pre-fetch the core activation record: the usable gate reads it,
        // but never while `inner` is held.
        let states = self.state.state_map();
        let inner = self.inner.read().expect("capability registry poisoned");
        let specs = inner
            .values()
            .filter(|entry| {
                states
                    .get(&entry.manifest.id)
                    .is_some_and(|state| state.activation.usable())
                    && !entry.tool_states.is_empty()
            })
            .flat_map(|entry| {
                entry
                    .tool_specs
                    .iter()
                    .filter(|spec| {
                        entry
                            .tool_states
                            .get(&spec.name)
                            .is_some_and(|state| state.lifecycle == ToolLifecycle::Loaded)
                    })
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
        // Pre-fetch the core activation record before the surface gate and
        // `inner`: the gate reads it without nesting the authority lock.
        let activation = self
            .state
            .activation(&owner)
            .unwrap_or(CapabilityActivation::Disabled);
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let entry = inner
            .get_mut(&owner)
            .ok_or_else(|| AgentError::Tool(format!("unknown tool: {tool_name}")))?;
        if !activation.usable() {
            return Err(AgentError::InvalidRequest(format!(
                "capability '{owner}' is {}; enable it before loading its tools",
                activation.as_str()
            )));
        }
        let tick = self.stamp_now();
        entry.tool_states.insert(
            tool_name.to_string(),
            CapabilityToolState {
                lifecycle: ToolLifecycle::Loaded,
                last_used_tick: tick,
            },
        );
        // A load puts tools on the surface: bump the generation.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Unified `capability.unload`: take one tool of the owning capability
    /// off the model surface. Siblings stay loaded. Roots only protect the
    /// idle path (like the builtin catalog): an explicit unload always
    /// works, and round-surface planning degrades per task demand.
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
            entry.tool_states.remove(tool_name);
        }
        // An unload takes tools off the surface: bump the generation.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Lifecycle state of one capability tool: `Active` while executing,
    /// `Loaded` when that tool itself is on the surface, `Warm`/`Unloaded`
    /// after idle cooling, `Available` otherwise. Sibling tools of the
    /// same capability report `Available` until they are loaded
    /// individually. A disabled/quarantined capability reports
    /// `Available` regardless — its tools are not on the surface.
    pub fn tool_state(&self, tool_name: &str) -> Option<ToolLifecycle> {
        // Pre-fetch the core activation record: the Loaded gate reads it
        // without nesting the authority lock inside the registry read.
        let states = self.state.state_map();
        let inner = self.inner.read().expect("capability registry poisoned");
        inner.values().find_map(|entry| {
            entry
                .tool_specs
                .iter()
                .any(|spec| spec.name == tool_name)
                .then(|| {
                    let usable = states
                        .get(&entry.manifest.id)
                        .is_some_and(|state| state.activation.usable());
                    if entry.active {
                        ToolLifecycle::Active
                    } else if usable {
                        entry
                            .tool_states
                            .get(tool_name)
                            .map(|state| state.lifecycle)
                            .unwrap_or(ToolLifecycle::Available)
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
            // Pre-fetch the core activation record: the Loaded gate reads
            // it without nesting the authority lock inside the registry
            // read.
            let states = self.state.state_map();
            let inner = self.inner.read().expect("capability registry poisoned");
            let mut rows = Vec::new();
            for (id, entry) in inner.iter() {
                let owner = id.clone();
                let active = entry.active;
                let usable = states
                    .get(id)
                    .is_some_and(|state| state.activation.usable());
                for spec in &entry.tool_specs {
                    let state = if active {
                        ToolLifecycle::Active
                    } else if usable {
                        entry
                            .tool_states
                            .get(&spec.name)
                            .map(|state| state.lifecycle)
                            .unwrap_or(ToolLifecycle::Available)
                    } else {
                        ToolLifecycle::Available
                    };
                    rows.push(ToolCatalogEntry {
                        name: spec.name.clone(),
                        state,
                        owner: owner.clone(),
                        description: spec.description.clone(),
                        risk: spec.risk,
                        roles: spec.effective_roles(),
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
    /// Executing a tool also refreshes its idle clock: a tool in active use
    /// never ages out mid-work.
    pub fn mark_active(&self, tool_name: &str) {
        let tick = self.stamp_now();
        let owner = self.owner_of(tool_name);
        if let Some(owner) = owner
            && let Some(entry) = self
                .inner
                .write()
                .expect("capability registry poisoned")
                .get_mut(&owner)
        {
            entry.active = true;
            if let Some(state) = entry.tool_states.get_mut(tool_name) {
                state.last_used_tick = tick;
            }
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

    /// The per-tool lifecycle safe point, called by the merged dispatcher
    /// once per model round (the same safe point that ages the builtin
    /// catalog). A loaded capability tool that stays idle cools
    /// Loaded -> Warm -> Unloaded exactly like a builtin tool; the active
    /// task's tool-demand set (`roots`) protects required capability tools
    /// from idle aging, so TaskAnchor-driven roots cover the whole unified
    /// surface, not just the builtin half.
    pub fn gc(&self, roots: &[String]) {
        // The single place this clock advances: once per model round, at
        // the same safe point that ages the builtin catalog.
        let tick = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
        let mut changed = false;
        {
            let mut inner = self.inner.write().expect("capability registry poisoned");
            for entry in inner.values_mut() {
                for (name, state) in entry.tool_states.iter_mut() {
                    if roots.iter().any(|root| root == name) {
                        continue;
                    }
                    let idle = tick.saturating_sub(state.last_used_tick);
                    match state.lifecycle {
                        ToolLifecycle::Loaded if idle >= self.idle_to_warm_ticks => {
                            state.lifecycle = ToolLifecycle::Warm;
                            changed = true;
                        }
                        ToolLifecycle::Warm if idle >= self.warm_to_unload_ticks => {
                            state.lifecycle = ToolLifecycle::Unloaded;
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.catalog_version.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot of every registered capability's surface state (activation +
    /// loaded tools), for checkpoints. Registration identity itself is not
    /// part of the snapshot: capabilities are re-registered by the
    /// composition root on a fresh run, then this re-applies their flags.
    pub fn snapshot(&self) -> Vec<crate::checkpoint::CapabilitySnapshot> {
        // Pre-fetch the core state record: the activation column reads it
        // without nesting the authority lock inside the registry read.
        let states = self.state.state_map();
        let inner = self.inner.read().expect("capability registry poisoned");
        let mut entries: Vec<_> = inner
            .iter()
            .map(|(id, entry)| {
                // Loaded and Warm tools both survive a checkpoint: Warm is
                // off the surface but retained in the catalog, so restore
                // brings it back ready. Unloaded/Available are not on the
                // surface and do not need a surface claim.
                let mut loaded_tools: Vec<String> = entry
                    .tool_states
                    .iter()
                    .filter(|(_, state)| {
                        matches!(state.lifecycle, ToolLifecycle::Loaded | ToolLifecycle::Warm)
                    })
                    .map(|(name, _)| name.clone())
                    .collect();
                loaded_tools.sort();
                crate::checkpoint::CapabilitySnapshot {
                    id: id.clone(),
                    // Fail closed: an entry without a core state record
                    // (unreachable by construction) snapshots as Disabled.
                    activation: states
                        .get(id)
                        .map(|state| state.activation)
                        .unwrap_or(CapabilityActivation::Disabled),
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

    /// Re-apply a checkpoint's capability surface state. Activation uses a
    /// fail-closed monotonic meet with current live authority, so an old
    /// Enabled checkpoint cannot undo a newer disable/quarantine. The
    /// loaded-tool surface is rebuilt only when that effective activation
    /// remains usable. Old whole-capability checkpoints
    /// (`loaded: true` with no tool list) migrate to "every declared tool
    /// loaded". Returns how many registered capability rows were actually
    /// applied; unknown ids do not count.
    pub fn restore(&self, state: &[crate::checkpoint::CapabilitySnapshot]) -> usize {
        let _surface = self
            .surface_gate
            .write()
            .expect("capability registry poisoned");
        // The surface gate freezes registry activation transitions, so this
        // one authority view stays valid while the mechanical surface is
        // rebuilt below without nesting authority and registry locks.
        let live_states = self.state.state_map();
        let mut inner = self.inner.write().expect("capability registry poisoned");
        let mut restored_states = Vec::with_capacity(state.len());
        for entry in state {
            let Some(current) = inner.get_mut(&entry.id) else {
                // The capability is not registered in this run; its flags
                // have nothing to apply to.
                continue;
            };
            let Some(live_state) = live_states.get(&entry.id).copied() else {
                // Registration guarantees a core state row. If that
                // invariant is ever broken, fail closed rather than
                // constructing a model-visible surface without authority.
                current.tool_states.clear();
                continue;
            };
            let activation = restore_activation_meet(live_state.activation, entry.activation);
            if activation.usable() && !entry.loaded_tools.is_empty() {
                // Per-tool format: only the named tools go on the surface,
                // and only those the capability actually declares in this
                // run (names unknown here are dropped).
                current.tool_states = entry
                    .loaded_tools
                    .iter()
                    .filter(|name| {
                        current
                            .tool_specs
                            .iter()
                            .any(|spec| spec.name.as_str() == *name)
                    })
                    .map(|name| {
                        (
                            name.clone(),
                            CapabilityToolState {
                                lifecycle: ToolLifecycle::Loaded,
                                last_used_tick: 0,
                            },
                        )
                    })
                    .collect();
            } else if activation.usable() && entry.loaded {
                // Legacy whole-capability format: everything was on the
                // surface in the checkpoint.
                current.tool_states = current
                    .tool_specs
                    .iter()
                    .map(|spec| {
                        (
                            spec.name.clone(),
                            CapabilityToolState {
                                lifecycle: ToolLifecycle::Loaded,
                                last_used_tick: 0,
                            },
                        )
                    })
                    .collect();
            } else {
                current.tool_states.clear();
            }
            restored_states.push((
                entry.id.clone(),
                CapabilityState {
                    status: live_state.status,
                    activation,
                },
            ));
        }
        drop(inner);
        let applied = restored_states.len();
        self.state.restore(&restored_states);
        // The restore attempt is a serialized surface epoch even when none
        // of its ids are registered. Preserve that generation fence while
        // reporting only actually matched rows to the restore event.
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.catalog_version.fetch_add(1, Ordering::Relaxed);
        applied
    }

    /// Start every eager capability that has not started yet (host start).
    /// Disabled/quarantined capabilities are not started — they are not
    /// usable, so there is nothing to run.
    pub async fn start_eager(&self) -> AgentResult<()> {
        // Pre-fetch the core activation record: the usable gate reads it
        // without nesting the authority lock inside the registry read.
        let states = self.state.state_map();
        let ids: Vec<String> = {
            let inner = self.inner.read().expect("capability registry poisoned");
            inner
                .iter()
                .filter(|(_, entry)| {
                    states
                        .get(&entry.manifest.id)
                        .is_some_and(|state| state.activation.usable())
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
        // Pre-fetch the core activation record before the inner read: the
        // usable gate below reads it without nesting the authority lock.
        let activation = self
            .state
            .activation(id)
            .unwrap_or(CapabilityActivation::Disabled);
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
            if !activation.usable() {
                return Err(AgentError::InvalidRequest(format!(
                    "capability '{id}' is {}; enable it before use",
                    activation.as_str()
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
    /// Host policy mapping (CORE-11) for deriving an admitted plugin's
    /// approved intent. Composition injects the same source the kernel
    /// lease path uses; `None` keeps the declared-risk empty bound, so an
    /// unadmitted plugin can never widen authority through this path.
    host_policies: Option<std::sync::Arc<dyn HostToolPolicies>>,
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
            host_policies: None,
        }
    }

    /// Install the host policy mapping (same source as the kernel config).
    pub fn with_host_policies(
        mut self,
        policies: std::sync::Arc<dyn HostToolPolicies>,
    ) -> Self {
        self.host_policies = Some(policies);
        self
    }

    /// The unified control tool schemas (always visible).
    fn control_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: CAPABILITY_MANAGE.into(),
                description: "Catalog ops: search, inspect, load, unload. Search by query and/or role=mutate|verify|read_resource|search|inspect_diff|escape_hatch. Load by exact name from the TOOL CATALOG index.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["op"],
                    "properties": {
                        "op": {"type": "string", "enum": ["search", "inspect", "load", "unload"]},
                        "name": {"type": "string", "description": "Exact tool name for inspect/load/unload"},
                        "query": {"type": "string", "description": "search: token match over name/description/owner/state/risk"},
                        "role": {
                            "type": "string",
                            "enum": ["read_resource", "search", "inspect_diff", "verify", "mutate", "escape_hatch"],
                            "description": "search: filter by semantic role instead of guessing keywords"
                        },
                        "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                        "cursor": {"type": "string"}
                    }
                }),
                risk: agent_contracts::ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
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
        // provider knows which entries are configured as core.
        // `capability.manage` stays fail-closed. `context.manage` follows
        // the builtin catalog (production default is catalog-only).
        if name == CAPABILITY_MANAGE {
            false
        } else if self.capabilities.owner_of(name).is_some() {
            true
        } else {
            self.base.may_omit_from_round(name)
        }
    }

    fn gc(&self, roots: &[String]) {
        // One unified safe point: the builtin catalog ages (with its
        // always-loaded core), and the capability registry ages its loaded
        // tools with the same thresholds and the same TaskAnchor roots —
        // external tools receive the same idle-cooling + root semantics as
        // builtins.
        self.capabilities.gc(roots);
        self.base.gc(roots);
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
        request.validate().map_err(AgentError::InvalidRequest)?;
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_MANAGE => self.run_manage(request).await.map(ToolOutcome::Value),
            _ => {
                if let Some(id) = self.capabilities.owner_of(&name) {
                    let effectful = self
                        .capabilities
                        .tool_spec(&name)
                        .is_some_and(|spec| spec.risk != agent_contracts::ToolRisk::ReadOnly);
                    if effectful != request.effect_context.is_some() {
                        return Err(AgentError::InvalidRequest(format!(
                            "capability tool '{name}' {} a Core-issued effect context",
                            if effectful {
                                "requires"
                            } else {
                                "must not receive"
                            }
                        )));
                    }
                    let capability = self
                        .capabilities
                        .by_tool(&name)
                        .ok_or_else(|| AgentError::Tool(format!("unknown tool: {name}")))?;
                    // The id and the grant come from the registry's records
                    // (the grant is a core record captured at admission),
                    // never from the live capability object: a capability
                    // that returns a different manifest after registration
                    // cannot escalate what it holds.
                    let grant = self
                        .capabilities
                        .granted_permissions(&id)
                        .ok_or_else(|| AgentError::Tool(format!("unknown tool: {name}")))?;
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
                    let ctx = self.invocation_context(&grant, &request);
                    let outcome = self
                        .invoke_capability_with_remote_barrier(capability.as_ref(), request, ctx)
                        .await;
                    self.capabilities.mark_idle(&name);
                    return Self::map_capability_outcome(&id, &grant, outcome);
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
    async fn read_bounded(
        &self,
        relative: &str,
        max_bytes: usize,
    ) -> AgentResult<agent_contracts::BoundedRead> {
        self.0.read_bounded(relative, max_bytes).await
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
    async fn read_bounded(
        &self,
        relative: &str,
        max_bytes: usize,
    ) -> AgentResult<agent_contracts::BoundedRead> {
        self.0.read_bounded(relative, max_bytes).await
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
    /// 有 workspace 且这次调用带 Core effect 身份时，先落下远程幂等屏障
    /// 再把请求交给子进程/MCP。没有屏障的崩溃窗口不得声称 at-most-one。
    async fn invoke_capability_with_remote_barrier(
        &self,
        capability: &dyn Capability,
        request: ToolExecutionRequest,
        ctx: CapabilityInvocationContext,
    ) -> AgentResult<CapabilityOutcome> {
        let Some((context, workspace)) =
            request.effect_context.as_ref().zip(self.workspace.as_ref())
        else {
            return capability.invoke(request.call, ctx).await;
        };
        workspace
            .record_remote_reserved(context, Some(&context.identity.operation_id.to_string()))?;
        workspace.record_remote_dispatched(context.effect_id)?;
        let outcome = capability.invoke(request.call, ctx).await;
        let ack = match &outcome {
            Ok(CapabilityOutcome::EffectRequest { .. }) => RemoteEffectAck::Staged,
            Ok(_) => RemoteEffectAck::Completed,
            Err(_) => RemoteEffectAck::Failed,
        };
        workspace.record_remote_acked(context.effect_id, ack)?;
        outcome
    }

    fn map_capability_outcome(
        id: &str,
        grant: &[String],
        outcome: AgentResult<CapabilityOutcome>,
    ) -> AgentResult<ToolOutcome> {
        // The core owns every side effect: a capability can only
        // stage an effect (the actor commits it behind the
        // generation fence) or attach a runtime directive —
        // which is refused unless the registered grant declares
        // `runtime:context-control`. A plain value passes
        // through unchanged.
        match outcome? {
            CapabilityOutcome::Value(output) => Ok(ToolOutcome::Value(output)),
            CapabilityOutcome::EffectRequest { output, effect } => {
                Ok(ToolOutcome::PreparedEffect { output, effect })
            }
            CapabilityOutcome::RuntimeDirective { output, directive } => {
                if grant
                    .iter()
                    .any(|permission| permission == RUNTIME_CONTEXT_CONTROL)
                {
                    Ok(ToolOutcome::RuntimeDirective { output, directive })
                } else {
                    Ok(ToolOutcome::Value(ToolOutput {
                        ok: false,
                        summary: format!(
                            "capability '{id}' attempted a runtime directive without '{}' permission",
                            RUNTIME_CONTEXT_CONTROL
                        ),
                        model_content: format!(
                            "runtime directive denied: capability '{id}' does not hold '{}' permission",
                            RUNTIME_CONTEXT_CONTROL
                        ),
                        ..output
                    }))
                }
            }
        }
    }

    /// Build the invocation context for one capability call from its
    /// *registered* grant (the admission-validated permissions), plus
    /// confined handles for everything the runtime actually wired in. A
    /// capability that declared no workspace permission gets no workspace
    /// handle at all.
    fn invocation_context(
        &self,
        grant: &[String],
        request: &ToolExecutionRequest,
    ) -> CapabilityInvocationContext {
        let declares = |permission: &str| grant.iter().any(|p| p == permission);

        let workspace = if declares(WORKSPACE_READ) || declares(WORKSPACE_WRITE) {
            self.workspace.as_ref().map(|workspace| {
                let handle: Arc<dyn WorkspaceHandle> = match &request.effect_context {
                    Some(effect_context) => {
                        Arc::new(ConfinedWorkspaceHandle::new_with_effect_context(
                            workspace,
                            &request.call.name,
                            effect_context.clone(),
                        ))
                    }
                    None => Arc::new(ConfinedWorkspaceHandle::new(workspace, &request.call.name)),
                };
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
        let artifacts = if grant.iter().any(|p| p.starts_with("artifact")) {
            self.workspace.as_ref().map(|workspace| {
                let handle: Arc<dyn ArtifactHandle> =
                    Arc::new(ArtifactStoreHandle::new(workspace, request.run_id));
                handle
            })
        } else {
            None
        };
        let approved_intent = self
            .inspect_tool(&request.call.name)
            .map(|spec| match &self.host_policies {
                Some(policies) => policies.effect_intent(&request.call, &spec),
                None => unbound_effect_intent(&spec),
            });
        CapabilityInvocationContext {
            granted_permissions: grant.to_vec(),
            workspace,
            artifacts,
            cancel: request.cancel.clone(),
            approved_intent,
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
    role: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(serde::Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    role: Option<String>,
    /// Max rows in the model-facing page (default 20, capped at 50).
    #[serde(default)]
    limit: Option<usize>,
    /// Opaque cursor for paging: the last tool name of the previous page.
    #[serde(default)]
    cursor: Option<String>,
}

/// One model-facing catalog row: `state\tname\tpurpose`. The purpose
/// line lets one search answer "what does this tool do" — the model
/// chains straight to load/invoke instead of an extra `inspect` round
/// per candidate. Owner stays in the metadata descriptors and the
/// inspect card; the name keeps its position (second tab field) for
/// cursor paging.
fn catalog_row_line(entry: &ToolCatalogEntry) -> String {
    format!(
        "{}\t{}\t{}",
        entry.state.as_str(),
        entry.name,
        compact_tool_purpose(&entry.description)
    )
}

impl CapabilityAwareDispatcher {
    /// One bounded line naming the tools already on the model surface.
    /// Tool observations leave the frame when their scope closes, so the
    /// loaded set is otherwise invisible after eviction and the model
    /// re-loads tools it already has (observed live: repeated
    /// `capability.manage op=load` within one cell).
    fn loaded_surface_trailer(&self) -> String {
        let catalog = self.unified_catalog();
        let mut names: Vec<&str> = catalog
            .iter()
            .filter(|entry| entry.state.in_surface())
            .map(|entry| entry.name.as_str())
            .collect();
        names.sort_unstable();
        names.truncate(16);
        (!names.is_empty())
            .then(|| format!("\nsession-loaded: {}", names.join(" ")))
            .unwrap_or_default()
    }

    async fn run_search(
        &self,
        request: ToolExecutionRequest,
        args: SearchArgs,
    ) -> AgentResult<ToolOutput> {
        let page_size = args
            .limit
            .unwrap_or(CAPABILITY_SEARCH_DEFAULT_LIMIT)
            .clamp(1, CAPABILITY_SEARCH_MAX_LIMIT);
        let role = ToolSemanticRole::parse_search_arg(args.role.as_deref())
            .map_err(AgentError::InvalidRequest)?;
        // 描述符索引检索：name/description/owner/state/risk，可按
        // ToolSemanticRole 过滤，避免模型猜关键词。
        let mut entries = search_tool_catalog_filtered(
            &self.unified_catalog(),
            args.query.as_deref(),
            role,
            usize::MAX,
        );
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
                        .map(catalog_row_line)
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
        let lines: Vec<String> = page.iter().map(catalog_row_line).collect();
        let mut model_content = if lines.is_empty() {
            "no tools match".to_string()
        } else {
            lines.join("\n")
        };
        model_content.push_str(&self.loaded_surface_trailer());
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("{} tools matched ({} on the model surface)", total, active),
            model_content,
            artifact_ref,
            metadata: json!({
                "op": "search",
                "kind": "tool",
                "total": total,
                "active": active,
                "returned": page.len(),
                "has_more": has_more,
                "descriptors": page.iter().map(|entry| ResourceDescriptor::from_tool(entry, None)).collect::<Vec<_>>(),
            }),
        })
    }

    async fn run_inspect(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        let Some(spec) = self.inspect_tool(&name) else {
            let mut metadata = agent_contracts::DiscoveryMiss::NotFound.to_metadata();
            metadata["op"] = json!("inspect");
            return Ok(ToolOutput {
                call_id: request.call.id,
                tool_name: CAPABILITY_MANAGE.into(),
                ok: false,
                summary: format!("unknown tool: {name}"),
                model_content: format!("unknown tool: {name}"),
                artifact_ref: None,
                metadata,
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
                "op": "inspect",
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
        // A repeat load for a tool already on the surface is a cheap
        // no-op: the model cannot see the loaded set after its earlier
        // observations were evicted, so treat the redundancy as expected.
        let already = self
            .unified_catalog()
            .iter()
            .find(|entry| entry.name == name)
            .is_some_and(|entry| entry.state.in_surface());
        if !already {
            self.load_tool(&name)?;
        }
        let trailer = self.loaded_surface_trailer();
        let (summary, model_content) = if already {
            (
                format!("already loaded: {name}"),
                format!("already loaded: {name} — no change{trailer}"),
            )
        } else {
            (
                format!("tool loaded: {name}"),
                format!("tool loaded: {name} — its schema is now offered to the model{trailer}"),
            )
        };
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary,
            model_content,
            artifact_ref: None,
            metadata: json!({"tool": name, "already_loaded": already}),
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
                    role: args.role,
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

#[cfg(test)]
mod tests;

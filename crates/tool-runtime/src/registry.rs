//! The builtin tool catalog with a real lifecycle.
//!
//! Instead of creating every tool up front and handing all schemas to the
//! model on every request, the dispatcher keeps a catalog of known tools and
//! an *active set*: only loaded tools appear in `specs()`. The model drives
//! the lifecycle through the always-visible control tools
//! (`capability.search` / `capability.load` / `capability.unload`) — load
//! `git.status` when the task needs git, and the GC cools and unloads tools
//! that stop being used. Execution of a catalog tool is always permitted;
//! the lifecycle gates the model surface, and the approval gate protects
//! actual side effects.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use agent_contracts::{
    AgentError, AgentResult, ResourceDescriptor, ToolCatalogEntry, ToolDispatcher,
    ToolExecutionAttribution, ToolExecutionPurpose, ToolExecutionRequest, ToolLeaseReconcileReport,
    ToolOutcome, ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec, ToolSurfaceSnapshot,
    VerificationReuse, reject_staged_effect_for_process_tool, search_tool_catalog_filtered,
};
use agent_process::kill_process_tree;
use agent_workspace::Workspace;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::tools::{
    ArtifactReadTool, CodeDiagnosticsTool, CodeSymbolsTool, ContextManageTool, EditPatchTool,
    EditReplaceTool, FsListTool, FsMkdirTool, FsReadTool, FsWriteTool, GitDiffTool, GitStatusTool,
    ProcessRunTool, ProcessSessionTool, SearchGrepTool, SessionRegistry, SessionSlot,
    ShellExecTool, TaskCompleteTool, TaskManageTool, Tool, VerificationRunTool,
};
use crate::{VERIFY_RUN_TOOL_NAME, VerificationRecipes};

/// Control tools are now defined by the unified catalog contract.
pub use agent_contracts::{CAPABILITY_MANAGE, ToolLifecycle};

/// Tuning knobs for the catalog lifecycle.
#[derive(Debug, Clone)]
pub struct ToolLifecycleConfig {
    /// Tools that stay loaded for the whole run (the model always sees them).
    pub always_loaded: Vec<String>,
    /// Idle model rounds before a loaded tool cools to Warm.
    pub idle_to_warm_ticks: usize,
    /// Idle model rounds before a warm tool is unloaded from the model
    /// surface.
    pub warm_to_unload_ticks: usize,
    /// 已加载 schema 的软高水位（字节估算）：低于它时不冷却任何可选
    /// 工具——保留一个紧凑 schema 数十轮的期望成本仍低于一次因重载
    /// 而多出的模型轮。0 表示永远视为超压（纯闲置语义）。
    pub surface_soft_high_bytes: usize,
    /// 开始冷却后回收到该水位即停止：滞回避免在阈值附近来回抖动。
    pub surface_low_watermark_bytes: usize,
}

impl Default for ToolLifecycleConfig {
    fn default() -> Self {
        // Production always-loaded coding surface. Compact, universal file
        // creation and read-only Git review join read/search/edit.patch:
        // repeated catalog-control rounds cost far more than their combined
        // ~190-token schemas. Shell/process, `edit.replace`,
        // `context.manage` and plugin tools remain catalog-loaded on demand.
        // Scripted eval fixtures,
        // `--compare-arm`, and context-bench/mech ops still pin write/edit
        // and `context.manage`; live coding compare reuses this default.
        Self {
            always_loaded: vec![
                "fs.list".into(),
                "fs.read".into(),
                "search.grep".into(),
                // Every tool that spills large output returns an
                // `artifact://` reference; artifact.read is the bounded
                // read side of that contract, so it must always be on the
                // surface.
                "artifact.read".into(),
                // Canonical revision-aware mutation. Not edit.replace /
                // shell / process.
                "edit.patch".into(),
                // Compact universal coding primitives. Surface visibility
                // grants no write/effect authority; normal host policy and
                // approval still gate execution.
                "fs.write".into(),
                "git.status".into(),
                "git.diff".into(),
                // Structured task closure. M15-window evidence (2026-08-28):
                // 10 behavioral-pass cells never closed because the model
                // never discovered this schema through the catalog. Presence
                // is not pressure — the completion acceptance gate remains
                // the sole closure authority and refuses premature or
                // unverified proposals with a typed per-turn warning, so
                // autonomy is unchanged and the cost is one compact schema.
                "task.complete".into(),
                // Catalog control plane: search/inspect/load/unload. The
                // merged retrieval tool (`context.manage`) is catalog-only
                // until EXTERNAL CONTEXT / NeedEvidence (item 24).
                CAPABILITY_MANAGE.into(),
            ],
            idle_to_warm_ticks: 8,
            warm_to_unload_ticks: 24,
            // 生产水位：全部内置 schema 合计约 6–9KB，远低于高水位，
            // 因此可选工具在正常任务里不再被冷却；超大目录仍受滞回
            // 约束。测试要旧行为时显式置 0。
            surface_soft_high_bytes: 18_000,
            surface_low_watermark_bytes: 9_000,
        }
    }
}

struct ToolEntry {
    schema_bytes: usize,
    tool: Arc<dyn Tool>,
    state: ToolLifecycle,
    last_used_tick: u64,
    /// An explicit host/operator load is a live source until unload. Runtime
    /// lease loads leave this false and are released by decision-boundary
    /// reconciliation when their typed roots disappear.
    persistent_load: bool,
}

pub struct BuiltinToolDispatcher {
    catalog: RwLock<HashMap<String, ToolEntry>>,
    config: ToolLifecycleConfig,
    /// Kept for `capability.search` artifact spill: a catalog that does not
    /// fit the model-facing page writes its full listing here.
    workspace: Workspace,
    /// The shared `process.session` registry, kept by the dispatcher so
    /// module teardown can drain every live session (see [`Drop`] and
    /// [`BuiltinToolDispatcher::shutdown_sessions`]).
    sessions: SessionRegistry,
    tick: AtomicU64,
    /// Bumped on every lifecycle change (load/unload/gc transitions), so a
    /// `ToolSurfaceSnapshot` is auditably identifiable.
    generation: AtomicU64,
    /// Immutable host recipe table. The matching host-effect policy is
    /// derived from this same value by the composition root.
    verification_recipes: Arc<VerificationRecipes>,
}

impl Drop for BuiltinToolDispatcher {
    fn drop(&mut self) {
        // Synchronous teardown complement to `shutdown_sessions`: kill
        // every live session's whole process tree. Drop cannot await, so
        // the direct children are reaped when their handles drop with the
        // registry; the explicit async drain is the bounded-reap path
        // callers with an await budget (tests, hosts) use.
        if let Ok(sessions) = self.sessions.try_lock() {
            for slot in sessions.values() {
                if let SessionSlot::Running(session) = slot {
                    kill_process_tree(session.pid);
                }
            }
        }
    }
}

impl BuiltinToolDispatcher {
    fn stays_loaded(&self, name: &str) -> bool {
        name == VERIFY_RUN_TOOL_NAME || self.config.always_loaded.iter().any(|core| core == name)
    }

    pub fn new(workspace: Workspace) -> Self {
        let recipes = VerificationRecipes::discover(&workspace);
        Self::with_config_and_verification_recipes(
            workspace,
            ToolLifecycleConfig::default(),
            recipes,
        )
    }

    pub fn with_config(workspace: Workspace, config: ToolLifecycleConfig) -> Self {
        let recipes = VerificationRecipes::discover(&workspace);
        Self::with_config_and_verification_recipes(workspace, config, recipes)
    }

    pub fn with_config_and_verification_recipes(
        workspace: Workspace,
        config: ToolLifecycleConfig,
        verification_recipes: VerificationRecipes,
    ) -> Self {
        let verification_recipes = Arc::new(verification_recipes);
        // One session registry per dispatcher, shared by every
        // `process.session` tool instance and kept by the dispatcher for
        // module-shutdown draining.
        let sessions = SessionRegistry::default();
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FsListTool::new(workspace.clone())),
            Arc::new(FsReadTool::new(workspace.clone())),
            Arc::new(ArtifactReadTool::new(workspace.clone())),
            Arc::new(FsWriteTool::new(workspace.clone())),
            Arc::new(FsMkdirTool::new(workspace.clone())),
            Arc::new(SearchGrepTool::new(workspace.clone())),
            Arc::new(EditReplaceTool::new(workspace.clone())),
            Arc::new(EditPatchTool::new(workspace.clone())),
            Arc::new(GitStatusTool::new(workspace.clone())),
            Arc::new(GitDiffTool::new(workspace.clone())),
            Arc::new(ShellExecTool::new(workspace.clone())),
            Arc::new(ProcessRunTool::new(workspace.clone())),
            Arc::new(ProcessSessionTool::new(workspace.clone(), sessions.clone())),
            // Local symbol/diagnostic navigation: catalog-optional
            // first-party tools loaded on demand when a task needs precise
            // navigation (pure local scans, no embeddings/vector storage).
            Arc::new(CodeSymbolsTool::new(workspace.clone())),
            Arc::new(CodeDiagnosticsTool::new(workspace.clone())),
            Arc::new(ContextManageTool::new()),
            Arc::new(TaskCompleteTool::new()),
            // Catalog-cold autonomous progress surface for explicit
            // long-task runs; ordinary turns discover it through the
            // catalog instead of paying its schema every round.
            Arc::new(TaskManageTool::new()),
        ];
        if let Some(tool) =
            VerificationRunTool::new(workspace.clone(), verification_recipes.clone())
        {
            tools.push(Arc::new(tool));
        }
        let catalog = tools
            .into_iter()
            .map(|tool| {
                let spec = tool.spec();
                let state = if config.always_loaded.contains(&spec.name)
                    || spec.name == VERIFY_RUN_TOOL_NAME
                {
                    ToolLifecycle::Loaded
                } else {
                    ToolLifecycle::Available
                };
                (
                    spec.name.clone(),
                    ToolEntry {
                        schema_bytes: serde_json::to_string(&spec)
                            .map(|text| text.len())
                            .unwrap_or(0),
                        tool,
                        state,
                        last_used_tick: 0,
                        persistent_load: false,
                    },
                )
            })
            .collect();
        Self {
            catalog: RwLock::new(catalog),
            config,
            workspace,
            sessions,
            tick: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            verification_recipes,
        }
    }

    /// Module shutdown: stop every live `process.session` child with the
    /// same teardown as `stop` (tree kill, bounded reap, artifact seal,
    /// exit persist). Idempotent; best effort across sessions.
    pub async fn shutdown_sessions(&self) -> AgentResult<()> {
        super::tools::drain_sessions(&self.workspace, &self.sessions).await
    }

    pub fn verification_recipes(&self) -> VerificationRecipes {
        self.verification_recipes.as_ref().clone()
    }

    /// Host/operator load. The schema remains resident until explicit unload;
    /// this is a surface source only and never grants execution authority.
    pub fn load(&self, name: &str) -> AgentResult<()> {
        let tick = self.stamp_now();
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let entry = catalog.get_mut(name).ok_or_else(|| {
            AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
        })?;
        entry.state = ToolLifecycle::Loaded;
        entry.last_used_tick = tick;
        entry.persistent_load = true;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Runtime lease load. An existing persistent source is preserved, but a
    /// new runtime load is eligible for source reconciliation.
    fn load_for_lease(&self, name: &str) -> AgentResult<()> {
        let tick = self.stamp_now();
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let entry = catalog.get_mut(name).ok_or_else(|| {
            AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
        })?;
        entry.state = ToolLifecycle::Loaded;
        entry.last_used_tick = tick;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Unload a tool from the model surface. Core tools in `always_loaded`
    /// cannot be unloaded.
    pub fn unload(&self, name: &str) -> AgentResult<()> {
        if self.stays_loaded(name) {
            return Err(AgentError::InvalidRequest(format!(
                "core tool '{name}' cannot be unloaded"
            )));
        }
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let entry = catalog.get_mut(name).ok_or_else(|| {
            AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
        })?;
        entry.state = ToolLifecycle::Unloaded;
        entry.persistent_load = false;
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Capture the current tool surface for one model round, with the
    /// catalog generation at capture time. The runtime calls this once per
    /// round, right after `gc()`.
    pub fn surface(&self) -> ToolSurfaceSnapshot {
        // Every surface mutation updates `generation` before releasing the
        // catalog write lock. Reading both under the matching read lock
        // therefore gives the snapshot one exact catalog revision instead
        // of pairing old specs with a newer generation.
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        let specs = Self::surface_specs(&catalog);
        let generation = self.generation.load(Ordering::Relaxed);
        ToolSurfaceSnapshot {
            specs,
            generation,
            source_revisions: agent_contracts::ToolSurfaceSourceRevisions {
                builtin_catalog_generation: generation,
                ..Default::default()
            },
            ..ToolSurfaceSnapshot::default()
        }
    }

    /// Snapshot of the catalog for `capability.search`.
    pub fn catalog(&self) -> Vec<ToolCatalogEntry> {
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        let mut entries: Vec<_> = catalog
            .iter()
            .map(|(name, entry)| {
                let spec = entry.tool.spec();
                let roles = spec.effective_roles();
                ToolCatalogEntry {
                    name: name.clone(),
                    state: entry.state,
                    owner: "builtin".to_string(),
                    description: spec.description,
                    risk: spec.risk,
                    roles,
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// Current lifecycle-clock value. Loads and executes STAMP entries
    /// with it; ONLY `gc` — the runtime's once-per-model-round safe point
    /// — advances the clock, so `idle_to_warm_ticks` really means "model
    /// rounds without use". A tool call must never make time pass faster.
    fn stamp_now(&self) -> u64 {
        self.tick.load(Ordering::Relaxed)
    }

    /// 当前 Loaded 状态条目的 schema 字节总量。统一表面驻留规划把它
    /// 与 capability 侧的字节数放进同一个压力预算。
    pub fn loaded_surface_bytes(&self) -> usize {
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        catalog
            .values()
            .filter(|entry| entry.state == ToolLifecycle::Loaded)
            .map(|entry| entry.schema_bytes)
            .sum()
    }

    /// Project runtime-owned leases onto the builtin surface. This is a
    /// decision-boundary transition, not idle GC: no clock advances and no
    /// threshold participates. Unrooted optional schemas become Warm while
    /// remaining discoverable and exactly reloadable.
    pub fn reconcile_leases(&self, roots: &[String]) -> ToolLeaseReconcileReport {
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let mut candidates: Vec<String> = catalog
            .iter()
            .filter(|(name, entry)| {
                entry.state == ToolLifecycle::Loaded && !self.stays_loaded(name)
            })
            .map(|(name, _)| name.clone())
            .collect();
        candidates.sort();

        let mut report = ToolLeaseReconcileReport::default();
        let mut changed = false;
        for name in candidates {
            let persistent = catalog
                .get(&name)
                .is_some_and(|entry| entry.persistent_load);
            if persistent {
                report.record_retained_persistent();
                continue;
            }
            if roots.iter().any(|root| root == &name) {
                report.record_retained();
                continue;
            }
            if let Some(entry) = catalog.get_mut(&name) {
                entry.state = ToolLifecycle::Warm;
                changed = true;
                report.record_released(&name);
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        report
    }

    /// Age transitions at an explicit runtime safe point: idle tools cool
    /// Loaded -> Warm and then Warm -> Unloaded, so the model surface
    /// tracks recent use. Core tools (`always_loaded`) never age out, and
    /// neither do the runtime's TaskAnchor-driven roots — the active task's
    /// tool-demand set — because a task that requires a tool must keep it
    /// available regardless of idle ticks. Called once per model round by
    /// the runtime — never implicitly from `specs()`, which must stay pure
    /// so budget, prompt and tool-call validation all observe one stable
    /// surface per round.
    pub fn gc(&self, roots: &[String]) {
        // The single place the lifecycle clock advances: once per model
        // round, at the runtime safe point.
        let tick = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let mut changed = false;
        // 表面压力滞回：已加载 schema 总量低于高水位时不冷却任何可选
        // 工具（保留成本 < 重载轮成本）；超高水位才按最久未用冷却到
        // 低水位为止。水位为 0 视为永远超压，即纯闲置语义。
        let loaded_bytes = |catalog: &HashMap<String, ToolEntry>| -> usize {
            catalog
                .values()
                .filter(|entry| entry.state == ToolLifecycle::Loaded)
                .map(|entry| entry.schema_bytes)
                .sum()
        };
        let mut pressure = if self.config.surface_soft_high_bytes > 0 {
            loaded_bytes(&catalog)
        } else {
            usize::MAX
        };
        let over_pressure = pressure > self.config.surface_soft_high_bytes;
        if over_pressure {
            let mut aging: Vec<(&String, &mut ToolEntry, usize)> = catalog
                .iter_mut()
                .filter_map(|(name, entry)| {
                    if self.stays_loaded(name)
                        || entry.persistent_load
                        || roots.iter().any(|root| root == name)
                        || entry.state != ToolLifecycle::Loaded
                    {
                        return None;
                    }
                    let idle = tick.saturating_sub(entry.last_used_tick) as usize;
                    (idle >= self.config.idle_to_warm_ticks).then_some((name, entry, idle))
                })
                .collect();
            aging.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(b.0)));
            for (_, entry, _) in aging {
                if pressure <= self.config.surface_low_watermark_bytes {
                    break;
                }
                let bytes = entry.schema_bytes;
                entry.state = ToolLifecycle::Warm;
                pressure = pressure.saturating_sub(bytes);
                changed = true;
            }
        }
        for (name, entry) in catalog.iter_mut() {
            if self.stays_loaded(name)
                || entry.persistent_load
                || roots.iter().any(|root| root == name)
            {
                continue;
            }
            let idle = tick.saturating_sub(entry.last_used_tick);
            match entry.state {
                ToolLifecycle::Warm if idle >= self.config.warm_to_unload_ticks as u64 => {
                    entry.state = ToolLifecycle::Unloaded;
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn meta_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: CAPABILITY_MANAGE.into(),
                description: "Tool-catalog ops: search, inspect, load, unload. Search by query and/or role=mutate|verify|read_resource|search|inspect_diff|escape_hatch. Load only an exact tool name from the TOOL CATALOG index; a verify.run recipe_id is an argument value, not a loadable tool.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["op"],
                    "properties": {
                        "op": {"type": "string", "enum": ["search", "inspect", "load", "unload"]},
                        "name": {"type": "string", "description": "Exact tool name for inspect/load/unload; never a verify.run recipe_id"},
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
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
        ]
    }

    fn surface_specs(catalog: &HashMap<String, ToolEntry>) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = catalog
            .values()
            .filter(|entry| entry.state.in_surface())
            .map(|entry| entry.tool.spec())
            .collect();
        specs.extend(Self::meta_specs());
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

#[async_trait::async_trait]
impl ToolDispatcher for BuiltinToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        // Pure read: lifecycle aging happens only at the explicit runtime
        // safe point (`gc()`), never here, so one model round sees a stable
        // tool surface for budget, prompt and validation alike.
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        Self::surface_specs(&catalog)
    }

    fn gc(&self, roots: &[String]) {
        // The explicit lifecycle safe point the runtime calls once per
        // model round; delegates to the inherent method so callers on the
        // concrete type and through the trait observe the same aging.
        Self::gc(self, roots);
    }

    fn reconcile_leases(&self, roots: &[String]) -> ToolLeaseReconcileReport {
        Self::reconcile_leases(self, roots)
    }

    fn loaded_surface_bytes(&self) -> usize {
        Self::loaded_surface_bytes(self)
    }

    fn snapshot(&self) -> ToolSurfaceSnapshot {
        self.surface()
    }

    fn may_omit_from_round(&self, name: &str) -> bool {
        // The catalog configuration is the authority for builtin core
        // membership. `capability.manage` is fail-closed even when a custom
        // configuration accidentally leaves it out of `always_loaded`.
        // `context.manage` follows `always_loaded` (catalog-only on the
        // production default; item 24). Unknown names stay fail-closed.
        if name == CAPABILITY_MANAGE {
            return false;
        }
        self.catalog
            .read()
            .expect("tool catalog poisoned")
            .contains_key(name)
            && !self.stays_loaded(name)
    }

    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        Self::catalog(self)
    }

    fn load_tool(&self, name: &str) -> AgentResult<()> {
        self.load(name)
    }

    fn load_tool_for_lease(&self, name: &str) -> AgentResult<()> {
        self.load_for_lease(name)
    }

    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        self.unload(name)
    }

    fn inspect_tool(&self, name: &str) -> Option<ToolSpec> {
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        catalog.get(name).map(|entry| entry.tool.spec())
    }

    fn execution_attribution(&self, call: &agent_contracts::ToolCall) -> ToolExecutionAttribution {
        self.builtin_execution_attribution(call)
    }

    /// translate this crate's OWN stamped outputs into typed
    /// facts at one sanctioned point inside the operator-trust boundary.
    /// Names this catalog does not own belong to untrusted producers routed
    /// elsewhere and contribute no facts.
    fn execution_facts(&self, output: &ToolOutput) -> agent_contracts::ToolExecutionFacts {
        let owned = self
            .catalog
            .read()
            .expect("tool catalog poisoned")
            .contains_key(&output.tool_name);
        if !owned {
            return agent_contracts::ToolExecutionFacts::empty();
        }
        // Handler-native facts win: the trusted producer stamped its own
        // execution truth at construction time. The legacy key derivation
        // stays only as the fallback for handlers that have not moved to
        // native stamping; per-handler tests lock both channels to agree.
        if let Some(native) = output.native_execution_facts() {
            return native;
        }
        Self::translate_stamped_execution_facts(output)
    }

    fn verification_equivalent(&self, left: (&str, &str), right: (&str, &str)) -> bool {
        self.verification_recipes.same_declared_class(left, right)
    }

    fn verification_coverage_declarations(
        &self,
    ) -> Vec<agent_contracts::VerificationCoverageDeclaration> {
        self.verification_recipes.coverage_declarations().to_vec()
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        request.validate().map_err(AgentError::InvalidRequest)?;
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_MANAGE => self.run_manage(request).await.map(ToolOutcome::Value),
            _ => {
                let tick = self.stamp_now();
                let tool = {
                    let mut catalog = self.catalog.write().expect("tool catalog poisoned");
                    let entry = catalog.get_mut(&name).ok_or_else(|| {
                        AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
                    })?;
                    let effectful = entry.tool.spec().risk != ToolRisk::ReadOnly;
                    if effectful != request.effect_context.is_some() {
                        return Err(AgentError::InvalidRequest(format!(
                            "tool '{name}' {} a Core-issued effect context",
                            if effectful {
                                "requires"
                            } else {
                                "must not receive"
                            }
                        )));
                    }
                    entry.state = ToolLifecycle::Active;
                    entry.last_used_tick = tick;
                    entry.tool.clone()
                };
                let output = tool
                    .execute(
                        request.run_id,
                        &request.call.id,
                        request.call.arguments,
                        request.effect_context,
                        request.cancel,
                    )
                    .await;
                let output = match output {
                    Ok(outcome) => reject_staged_effect_for_process_tool(&name, outcome),
                    Err(error) => Err(error),
                };
                let mut catalog = self.catalog.write().expect("tool catalog poisoned");
                if let Some(entry) = catalog.get_mut(&name)
                    && entry.state == ToolLifecycle::Active
                {
                    entry.state = ToolLifecycle::Loaded;
                }
                output
            }
        }
    }
}

/// Trusted builtin call-purpose mapping. Only workspace resource identities
/// are copied; arbitrary command strings, queries and content never enter the
/// attribution channel. Generic shell/process remain opaque even when their
/// command happens to run tests; only a host recipe may become Verify.
impl BuiltinToolDispatcher {
    /// Explicit bound for the catalog's own control-surface results;
    /// mirrors `tools::builtin_bound` so every capability.manage outcome
    /// carries native facts instead of the name-table fallback.
    fn manage_bound(may_mutate_workspace: bool) -> agent_contracts::ToolExecutionFacts {
        agent_contracts::ToolExecutionFacts::empty()
            .with_verification(false)
            .with_mutation_bound(may_mutate_workspace)
    }

    /// Legacy derivation from this crate's stamped producer-authority keys.
    /// Fallback for outputs whose handler has not moved to native stamping;
    /// per-handler tests lock the native channel to agree with this.
    pub(crate) fn translate_stamped_execution_facts(
        output: &ToolOutput,
    ) -> agent_contracts::ToolExecutionFacts {
        agent_contracts::ToolExecutionFacts::from_resource_touches(
            output
                .resource_touches()
                .into_iter()
                .map(|touch| (touch.path, touch.revision)),
        )
        .with_verification(output.is_verification())
        .with_mutation_bound(output.may_mutate_workspace())
    }

    fn builtin_execution_attribution(
        &self,
        call: &agent_contracts::ToolCall,
    ) -> ToolExecutionAttribution {
        if call.name == VERIFY_RUN_TOOL_NAME {
            let Some(recipe_id) = call
                .arguments
                .get("recipe_id")
                .and_then(|value| value.as_str())
                .map(str::trim)
            else {
                return ToolExecutionAttribution::default();
            };
            let Some(recipe) = self.verification_recipes.get(recipe_id) else {
                return ToolExecutionAttribution::default();
            };
            let attribution = ToolExecutionAttribution::bounded(
                ToolExecutionPurpose::Verify,
                recipe.cwd.clone().or_else(|| Some(".".into())),
                recipe.reuse,
            );
            if recipe.reuse != VerificationReuse::ExactCurrentWorld {
                return attribution;
            }
            let Some(exact_identity) = crate::verification::recipe_exact_identity(
                &self.verification_recipes,
                recipe,
                &self.workspace,
            ) else {
                // Exact equivalence could not be captured completely (for
                // example an oversized inherited environment). Keep typed
                // verification, but execute every request.
                return attribution;
            };
            let identity_material = &exact_identity.material;
            let executable_identity = &exact_identity.executable_identity;
            let class_identity_digest = recipe
                .coverage_domain
                .as_ref()
                .and_then(|_| {
                    self.verification_recipes.class_shared_identity(
                        recipe,
                        &self.workspace.runtime_facts(),
                        self.workspace.root(),
                        executable_identity,
                    )
                })
                .map(|material| format!("sha256-{:x}", Sha256::digest(material.trim().as_bytes())))
                .unwrap_or_default();
            let domain_declaration = recipe
                .coverage_domain
                .as_deref()
                .and_then(|domain| self.verification_recipes.coverage_declaration(domain));
            let provenance = agent_contracts::VerificationRecipeProvenance {
                recipe_id: recipe.id.clone(),
                recipe_revision: recipe.revision.clone(),
                coverage_domain: recipe.coverage_domain.clone(),
                domain_declaration_revision: domain_declaration
                    .map(|declared| declared.declaration_revision),
                domain_source_digest: domain_declaration
                    .map(|declared| declared.source_digest.clone())
                    .unwrap_or_default(),
                class_identity_digest,
            };
            return attribution
                .with_verification_identity_material(identity_material)
                .with_verification_recipe(provenance);
        }
        let purpose = match call.name.as_str() {
            "fs.read" => ToolExecutionPurpose::Observe,
            "fs.list" | "search.grep" | "code.symbols" | "code.diagnostics" => {
                ToolExecutionPurpose::Search
            }
            "fs.write" | "fs.mkdir" | "edit.replace" | "edit.patch" => ToolExecutionPurpose::Mutate,
            "shell.exec" | "process.run" | "process.session" => ToolExecutionPurpose::Opaque,
            CAPABILITY_MANAGE | "context.manage" | "task.complete" => ToolExecutionPurpose::Control,
            "git.status" | "git.diff" | "artifact.read" => ToolExecutionPurpose::Observe,
            _ => ToolExecutionPurpose::Unattributed,
        };
        let mut targets = Vec::new();
        if matches!(
            call.name.as_str(),
            "fs.read"
                | "fs.list"
                | "search.grep"
                | "fs.write"
                | "fs.mkdir"
                | "edit.replace"
                | "git.diff"
                | "code.symbols"
                | "code.diagnostics"
        ) && let Some(path) = call.arguments.get("path").and_then(|value| value.as_str())
        {
            targets.push(path.to_string());
        }
        if call.name == "edit.patch"
            && let Some(files) = call
                .arguments
                .get("files")
                .and_then(|value| value.as_array())
        {
            targets.extend(files.iter().filter_map(|file| {
                file.get("path")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            }));
        }
        ToolExecutionAttribution::bounded(purpose, targets, VerificationReuse::None)
    }
}

impl BuiltinToolDispatcher {
    async fn run_search(
        &self,
        request: ToolExecutionRequest,
        args: SearchArgs,
    ) -> AgentResult<ToolOutput> {
        let page_size = args
            .limit
            .unwrap_or(agent_contracts::CAPABILITY_SEARCH_DEFAULT_LIMIT)
            .clamp(1, agent_contracts::CAPABILITY_SEARCH_MAX_LIMIT);
        let role = ToolSemanticRole::parse_search_arg(args.role.as_deref())
            .map_err(AgentError::InvalidRequest)?;
        // Token-OR catalog search over name/description/owner/state/risk,
        // optionally filtered by ToolSemanticRole so the model can ask
        // for mutate/verify instead of guessing keywords.
        let mut entries =
            search_tool_catalog_filtered(&self.catalog(), args.query.as_deref(), role, usize::MAX);
        let active = entries
            .iter()
            .filter(|entry| entry.state.in_surface())
            .count();
        let total = entries.len();
        // A catalog that does not fit the page spills its full listing to
        // an artifact — the model only ever sees the bounded page, so a
        // large tool catalog cannot itself become context pollution.
        let artifact_ref = if total > page_size {
            let all: String = entries
                .iter()
                .map(|entry| {
                    format!(
                        "{}\t{}\t{}",
                        entry.state.as_str(),
                        entry.name,
                        agent_contracts::compact_tool_purpose(&entry.description)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(
                self.workspace
                    .write_artifact(request.run_id, "capability-search", "txt", all.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        if let Some(cursor) = args.cursor.as_deref() {
            entries.retain(|entry| entry.name.as_str() > cursor);
        }
        let remaining = entries.len();
        let page: Vec<_> = entries.into_iter().take(page_size).collect();
        let has_more = remaining > page.len();
        // `state\tname\tpurpose` — the purpose line lets one search answer
        // "what does this tool do", collapsing the browse-then-inspect
        // round trip. The name keeps its position (second tab field) for
        // cursor paging.
        let lines: Vec<String> = page
            .iter()
            .map(|entry| {
                format!(
                    "{}\t{}\t{}",
                    entry.state.as_str(),
                    entry.name,
                    agent_contracts::compact_tool_purpose(&entry.description)
                )
            })
            .collect();
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("{} tools matched ({} in the active set)", total, active),
            model_content: if lines.is_empty() {
                "no tools match".to_string()
            } else {
                lines.join("\n")
            },
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
        }
        .with_native_execution_facts(Self::manage_bound(false)))
    }

    async fn run_load(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        self.load_for_lease(&name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool loaded: {name}"),
            model_content: format!("tool loaded: {name} — its schema is now offered to the model"),
            artifact_ref: None,
            metadata: json!({"op": "load", "tool": name}),
        }
        .with_native_execution_facts(Self::manage_bound(false)))
    }

    async fn run_unload(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        self.unload(&name)?;
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool unloaded: {name}"),
            model_content: format!("tool unloaded: {name}"),
            artifact_ref: None,
            metadata: json!({"op": "unload", "tool": name}),
        }
        .with_native_execution_facts(Self::manage_bound(false)))
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
            }
            .with_native_execution_facts(Self::manage_bound(false)));
        };
        let state = self
            .catalog()
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.state.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_MANAGE.into(),
            ok: true,
            summary: format!("tool {name}: {state}"),
            model_content: format!(
                "name: {}\nowner: builtin\nstate: {}\ndescription: {}\nschema: {}",
                spec.name, state, spec.description, spec.input_schema
            ),
            artifact_ref: None,
            metadata: json!({"op": "inspect", "name": spec.name, "owner": "builtin", "state": state}),
        }
        .with_native_execution_facts(Self::manage_bound(false)))
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
mod tests {
    use super::*;
    use agent_contracts::CONTEXT_MANAGE;
    use agent_contracts::SchemaProfile;
    use agent_contracts::{CancellationToken, ContextAction, ToolCall, ToolExecutionRequest};
    use serde_json::Value;

    /// Unwrap a plain tool value (control tools never stage an effect or a
    /// directive).
    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("control tools return plain values"),
        }
    }

    /// Open a throwaway workspace whose directory outlives the returned
    /// dispatcher. Paging tests do execute artifact writes, so dropping the
    /// `TempDir` here would leave a dangling Workspace path.
    async fn open_workspace() -> (Workspace, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        (workspace, dir)
    }

    fn host_echo_recipe() -> crate::VerificationRecipe {
        #[cfg(windows)]
        let argv = vec!["cmd".into(), "/C".into(), "echo".into(), "trusted".into()];
        #[cfg(not(windows))]
        let argv = vec!["echo".into(), "trusted".into()];
        crate::VerificationRecipe::new("echo.trusted", "Echo trusted marker", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse()
    }

    struct TestDispatcher {
        inner: BuiltinToolDispatcher,
        _dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestDispatcher {
        type Target = BuiltinToolDispatcher;

        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }

    async fn dispatcher() -> TestDispatcher {
        let (workspace, dir) = open_workspace().await;
        TestDispatcher {
            inner: BuiltinToolDispatcher::new(workspace),
            _dir: dir,
        }
    }

    fn request(name: &str, arguments: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id: agent_contracts::RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    fn surface(dispatcher: &BuiltinToolDispatcher) -> Vec<String> {
        dispatcher
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    /// The host proof lane and the model-lane attribution must mint the
    /// same identity for the same recipe/world; otherwise the completion
    /// gate's identity fence can never agree with the observation it
    /// records.
    #[tokio::test]
    async fn host_proof_identity_agrees_with_dispatcher_attribution() {
        let (workspace, dir) = open_workspace().await;
        let recipes = Arc::new(VerificationRecipes::new(vec![host_echo_recipe()]).unwrap());
        let inner = BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            ToolLifecycleConfig::default(),
            (*recipes).clone(),
        );
        let dispatcher = TestDispatcher { inner, _dir: dir };
        let runner = crate::RecipeProofRunner::new(workspace, recipes).unwrap();

        let call = ToolCall {
            id: "c".into(),
            name: VERIFY_RUN_TOOL_NAME.into(),
            arguments: serde_json::json!({"recipe_id": "echo.trusted"}),
        };
        let stamped = dispatcher
            .builtin_execution_attribution(&call)
            .exact_verification_identity()
            .expect("exact recipe attribution must carry an identity")
            .to_string();
        assert_eq!(
            runner.exact_identity("echo.trusted").unwrap(),
            stamped,
            "host proof and model attribution must agree on the identity"
        );
    }

    /// Catalog meta-validation: every builtin `input_schema` (loaded or
    /// catalog-cold) must compile into the bounded central `SchemaProfile`.
    /// A schema that used an unsupported keyword or an unbounded shape would
    /// fail capability admission here instead of silently skipping
    /// validation.
    #[tokio::test]
    async fn every_builtin_schema_compiles_into_a_profile() {
        let tools = dispatcher().await;
        let catalog = tools.inner.catalog.read().expect("tool catalog poisoned");
        let specs: Vec<ToolSpec> = catalog.values().map(|entry| entry.tool.spec()).collect();
        assert!(
            specs.len() >= 12,
            "catalog unexpectedly small: {}",
            specs.len()
        );
        for spec in &specs {
            SchemaProfile::compile(&spec.input_schema).unwrap_or_else(|error| {
                panic!("builtin '{}' schema must compile: {error}", spec.name)
            });
        }
    }

    /// Shared corpus: for read-only builtins the central validator and the
    /// real tool agree — arguments the profile rejects never produce a
    /// successful dispatch, and arguments it accepts do.
    #[tokio::test]
    async fn shared_corpus_validator_agrees_with_builtin_parsers() {
        let tools = dispatcher().await;
        let surfaced: Vec<String> = tools.specs().into_iter().map(|spec| spec.name).collect();
        let profile_for = |name: &str| {
            SchemaProfile::compile(
                &tools
                    .specs()
                    .into_iter()
                    .find(|spec| spec.name == name)
                    .expect("row tool must be surfaced")
                    .input_schema,
            )
            .expect("builtin schema compiles")
        };
        let surfaced = |name: &str| surfaced.iter().any(|present| present == name);

        fn successful(outcome: &AgentResult<ToolOutcome>) -> bool {
            matches!(outcome, Ok(ToolOutcome::Value(output)) if output.ok)
        }

        // Bad shapes: wrong type, out-of-bounds, extra fields on a closed
        // object, and missing required keys. Every row must fail the central
        // validator and never produce a successful dispatch.
        let bad_rows: Vec<(&str, Value)> = vec![
            ("fs.read", serde_json::json!({"path": 7})),
            ("fs.list", serde_json::json!({"path": 5})),
            ("search.grep", serde_json::json!({"pattern": 3})),
            (
                "search.grep",
                serde_json::json!({"pattern": "x", "limit": 0}),
            ),
            ("fs.mkdir", serde_json::json!({"path": "", "extra": 1})),
            ("edit.patch", serde_json::json!({"path": []})),
        ];
        for (name, arguments) in bad_rows {
            if !surfaced(name) {
                continue;
            }
            let profile = profile_for(name);
            let validation = profile.validate(&arguments);
            assert!(
                validation.is_err(),
                "'{name}' arguments must fail the central validator: {arguments}"
            );
            let outcome = tools.execute(request(name, arguments)).await;
            assert!(
                !successful(&outcome),
                "'{name}' with schema-invalid arguments must not dispatch successfully: {outcome:?}"
            );
        }

        // Good shapes: on-surface arguments pass the validator and reach a
        // real dispatch.
        let good_rows: Vec<(&str, Value)> = vec![
            ("fs.read", serde_json::json!({"path": ".", "start_line": 1})),
            ("fs.list", serde_json::json!({"path": ""})),
            (
                "search.grep",
                serde_json::json!({"pattern": "x", "limit": 5}),
            ),
        ];
        for (name, arguments) in good_rows {
            if !surfaced(name) {
                continue;
            }
            let profile = profile_for(name);
            let validation = profile.validate(&arguments);
            assert!(
                validation.is_ok(),
                "'{name}' valid row rejected: {validation:?}"
            );
            let _ = tools.execute(request(name, arguments)).await;
        }
    }

    /// the trusted host translates only its own stamped
    /// outputs, and the translation mirrors what the legacy accessors
    /// derived — consumers switching to facts must not see new values.
    #[tokio::test]
    async fn execution_facts_translate_only_this_hosts_own_stamps() {
        let tools = dispatcher().await;

        let stamped = ToolOutput {
            call_id: "call-1".into(),
            tool_name: "fs.write".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({
                "path": "src/auth.rs",
                "revision": "r1",
                "mutates_workspace": true,
                "verification": false
            }),
        };
        let facts = tools.execution_facts(&stamped);
        assert_eq!(facts.resource_touches().len(), 1);
        assert_eq!(facts.resource_touches()[0].path, "src/auth.rs");
        assert_eq!(facts.resource_touches()[0].revision.as_deref(), Some("r1"));
        assert_eq!(facts.may_mutate_workspace(), Some(true));
        assert_eq!(facts.is_verification(), Some(false));
        assert!(
            matches!(
                facts.mutation_footprint(true, false),
                agent_contracts::MutationFootprint::Known(ref touches) if touches.len() == 1
            ),
            "the stamped bound drives the footprint without the name-table fallback"
        );

        // An unstamped read-only builtin still carries its bound from the
        // trusted host's own policy table, with no touches.
        let plain = ToolOutput {
            call_id: "call-2".into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({}),
        };
        let facts = tools.execution_facts(&plain);
        assert!(facts.resource_touches().is_empty());
        assert_eq!(facts.may_mutate_workspace(), Some(false));

        // A name outside this catalog belongs to another host's producer:
        // no facts, and the mutation bound stays unstamped.
        let foreign = ToolOutput {
            call_id: "call-3".into(),
            tool_name: "plugin.demo.one".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({"path": "escaped.rs", "verification": true}),
        };
        let facts = tools.execution_facts(&foreign);
        assert!(facts.resource_touches().is_empty());
        assert_eq!(facts.may_mutate_workspace(), None);
        assert_eq!(facts.is_verification(), None);
    }

    /// Handler-native stamps win over legacy-key derivation for owned
    /// tools; an unowned name cannot mint facts even from a native key —
    /// presence implies a trusted producer lane, not just a stamped key.
    #[tokio::test]
    async fn execution_facts_prefer_native_stamps_inside_the_trust_boundary() {
        let tools = dispatcher().await;

        let mut owned = ToolOutput {
            call_id: "call-4".into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({"path": "derived.rs", "revision": "r1"}),
        };
        owned.set_native_execution_facts(
            agent_contracts::ToolExecutionFacts::from_resource_touches([(
                "native.rs",
                Some("r9".to_owned()),
            )])
            .with_mutation_bound(false),
        );
        let facts = tools.execution_facts(&owned);
        assert_eq!(facts.resource_touches().len(), 1);
        assert_eq!(facts.resource_touches()[0].path, "native.rs");
        assert_eq!(facts.resource_touches()[0].revision.as_deref(), Some("r9"));
        assert_eq!(facts.may_mutate_workspace(), Some(false));

        let mut foreign = ToolOutput {
            call_id: "call-5".into(),
            tool_name: "plugin.demo.one".into(),
            ok: true,
            summary: "ok".into(),
            model_content: String::new(),
            artifact_ref: None,
            metadata: serde_json::json!({}),
        };
        foreign.set_native_execution_facts(
            agent_contracts::ToolExecutionFacts::empty()
                .with_verification(true)
                .with_mutation_bound(false),
        );
        let facts = tools.execution_facts(&foreign);
        assert!(facts.resource_touches().is_empty());
        assert_eq!(facts.may_mutate_workspace(), None);
        assert_eq!(facts.is_verification(), None);
    }

    /// Producer-bound coverage: representative outputs from migrated
    /// builtin families (control surface, git, search) carry native facts
    /// equal to the legacy derivation.
    #[tokio::test]
    async fn execution_facts_native_stamps_match_derivation_across_builtin_families() {
        let tools = dispatcher().await;
        let mut checked = 0usize;

        for outcome in [
            tools
                .execute(request(
                    "capability.manage",
                    serde_json::json!({"op": "inspect", "name": "fs.read"}),
                ))
                .await
                .unwrap(),
            tools
                .execute(request("git.status", serde_json::json!({})))
                .await
                .unwrap(),
            tools
                .execute(request(
                    "search.grep",
                    serde_json::json!({"pattern": "no_such_marker_xyz", "path": "."}),
                ))
                .await
                .unwrap(),
        ] {
            let ToolOutcome::Value(output) = outcome else {
                panic!("these read-only calls must return value outcomes");
            };
            let native = output
                .native_execution_facts()
                .expect("migrated builtin must stamp native facts");
            let derived = BuiltinToolDispatcher::translate_stamped_execution_facts(&output);
            assert_eq!(
                serde_json::to_value(&native).unwrap(),
                serde_json::to_value(&derived).unwrap(),
                "native facts diverge for {}",
                output.tool_name
            );
            checked += 1;
        }
        assert_eq!(checked, 3);
    }

    #[tokio::test]
    async fn default_surface_is_the_core_set_plus_control_tools() {
        let dispatcher = dispatcher().await;
        let names = surface(&dispatcher);
        assert!(names.contains(&"fs.read".to_string()));
        assert!(
            !names.contains(&"context.manage".to_string()),
            "item 24: context.manage is catalog-only on the production surface: {names:?}"
        );
        assert!(names.contains(&"capability.manage".to_string()));
        assert!(
            names.contains(&"task.complete".to_string()),
            "closure discovery is on the surface; the completion acceptance gate              remains the sole closure authority: {names:?}"
        );
        assert!(
            !names.contains(&"context.gc_hint".to_string()),
            "the meta-tools must be merged into context.manage: {names:?}"
        );
        assert!(
            !names.contains(&"capability.search".to_string()),
            "the catalog control tools must be merged into capability.manage: {names:?}"
        );
        assert!(names.contains(&"git.status".to_string()));
        assert!(names.contains(&"git.diff".to_string()));
        assert!(names.contains(&"fs.write".to_string()));
        assert!(
            !names.contains(&"fs.mkdir".to_string()),
            "directory topology stays catalog-cold until the measured surface gate"
        );
        assert!(
            dispatcher
                .catalog()
                .iter()
                .any(|entry| entry.name == "fs.mkdir"),
            "fs.mkdir must remain discoverable even while absent from the default surface"
        );
    }

    #[tokio::test]
    async fn discovered_verifier_is_visible_and_host_attributed_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn value() -> u8 { 1 }\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let dispatcher = BuiltinToolDispatcher::new(workspace);
        assert!(surface(&dispatcher).contains(&VERIFY_RUN_TOOL_NAME.to_string()));

        let call = ToolCall {
            id: "verify-1".into(),
            name: VERIFY_RUN_TOOL_NAME.into(),
            arguments: json!({"recipe_id": "rust.compile-tests:src/lib.rs"}),
        };
        let attribution = dispatcher.execution_attribution(&call);
        assert_eq!(attribution.purpose, ToolExecutionPurpose::Verify);
        assert_eq!(
            attribution.verification_reuse,
            VerificationReuse::ExactCurrentWorld
        );
        assert!(attribution.exact_verification_identity().is_some());
        assert_eq!(
            attribution,
            dispatcher.execution_attribution(&call),
            "stable host/executable world must produce one exact identity"
        );
        std::fs::write(dir.path().join("src/lib.rs"), "fn value() -> u8 { 2 }\n").unwrap();
        assert_ne!(
            attribution.verification_identity,
            dispatcher
                .execution_attribution(&call)
                .verification_identity,
            "an exact recipe input changed outside Runtime must invalidate PASS"
        );

        let opaque = dispatcher.execution_attribution(&ToolCall {
            id: "opaque".into(),
            name: "process.run".into(),
            arguments: json!({"argv": ["rustc", "--test", "src/lib.rs"]}),
        });
        assert_eq!(opaque.purpose, ToolExecutionPurpose::Opaque);
        assert_eq!(opaque.verification_reuse, VerificationReuse::None);
    }

    #[tokio::test]
    async fn dispatcher_projects_and_stamps_the_same_coverage_declaration_identity() {
        let (workspace, _dir) = open_workspace().await;
        let recipe = crate::VerificationRecipe::new(
            "project.check",
            "Check project",
            "v1",
            vec!["rustc".into(), "--version".into()],
        )
        .unwrap()
        .with_exact_current_world_reuse()
        .with_coverage_domain("workspace-tests")
        .unwrap();
        let recipes = VerificationRecipes::new(vec![recipe])
            .unwrap()
            .with_domains(vec![crate::VerificationCoverageDomain {
                domain_id: "workspace-tests".into(),
                declaration_revision: 4,
                members: vec!["project.check".into()],
            }])
            .unwrap();
        let expected = recipes.coverage_declarations()[0].clone();
        let dispatcher = BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace,
            ToolLifecycleConfig::default(),
            recipes,
        );
        assert_eq!(
            dispatcher.verification_coverage_declarations(),
            vec![expected.clone()]
        );
        let attribution = dispatcher.execution_attribution(&ToolCall {
            id: "verify".into(),
            name: VERIFY_RUN_TOOL_NAME.into(),
            arguments: json!({"recipe_id": "project.check"}),
        });
        let provenance = attribution.verification_recipe().unwrap();
        assert_eq!(provenance.domain_declaration_revision, Some(4));
        assert_eq!(provenance.domain_source_digest, expected.source_digest);
    }

    #[tokio::test]
    async fn general_project_test_recipe_is_typed_but_not_exactly_reused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let dispatcher = BuiltinToolDispatcher::new(workspace);
        let attribution = dispatcher.execution_attribution(&ToolCall {
            id: "verify-cargo".into(),
            name: VERIFY_RUN_TOOL_NAME.into(),
            arguments: json!({"recipe_id": "rust.workspace"}),
        });
        assert_eq!(attribution.purpose, ToolExecutionPurpose::Verify);
        assert_eq!(
            attribution.verification_reuse,
            VerificationReuse::TaskScoped
        );
        assert!(attribution.exact_verification_identity().is_none());
    }

    #[tokio::test]
    async fn exact_verification_identity_changes_with_host_recipe_revision() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let build = |revision: &str| {
            let recipe = crate::VerificationRecipe::new(
                "project.check",
                "Check project",
                revision,
                vec!["rustc".into(), "--version".into()],
            )
            .unwrap()
            .with_exact_current_world_reuse();
            BuiltinToolDispatcher::with_config_and_verification_recipes(
                workspace.clone(),
                ToolLifecycleConfig::default(),
                VerificationRecipes::new(vec![recipe]).unwrap(),
            )
        };
        let call = ToolCall {
            id: "verify".into(),
            name: VERIFY_RUN_TOOL_NAME.into(),
            arguments: json!({"recipe_id": "project.check"}),
        };
        let first = build("v1").execution_attribution(&call);
        let second = build("v2").execution_attribution(&call);
        assert_ne!(
            first.verification_identity, second.verification_identity,
            "a host recipe change must invalidate prior exact PASS"
        );
    }

    #[tokio::test]
    async fn context_manage_is_catalog_only_on_the_production_surface() {
        let dispatcher = dispatcher().await;
        assert!(
            !surface(&dispatcher).contains(&CONTEXT_MANAGE.to_string()),
            "item 24: production default must not always-load context.manage"
        );
        assert!(
            dispatcher
                .catalog()
                .iter()
                .any(|entry| entry.name == CONTEXT_MANAGE),
            "context.manage must remain catalog-loadable"
        );
        assert!(dispatcher.may_omit_from_round(CONTEXT_MANAGE));
        dispatcher.load(CONTEXT_MANAGE).unwrap();
        assert!(surface(&dispatcher).contains(&CONTEXT_MANAGE.to_string()));
        dispatcher.unload(CONTEXT_MANAGE).unwrap();
        assert!(!surface(&dispatcher).contains(&CONTEXT_MANAGE.to_string()));
    }

    #[tokio::test]
    async fn context_manage_search_names_the_whole_catalog() {
        let dispatcher = dispatcher().await;
        let spec = dispatcher
            .inspect_tool(CONTEXT_MANAGE)
            .expect("context.manage stays in the catalog");
        assert!(
            spec.description.contains("whole catalog")
                && spec.description.contains("Resident/Warm/Stored"),
            "search affordance must say the catalog is larger than the selected frame: {}",
            spec.description
        );
        assert!(
            !spec.description.contains("prefer") && !spec.description.contains("instead of"),
            "schema states coverage, not a retrieval tutorial: {}",
            spec.description
        );
        let query = spec.input_schema["properties"]["query"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(
            query.contains("path") && query.contains("entity"),
            "query key stays `query` and names the indexed fields: {query}"
        );
        assert_eq!(
            spec.input_schema["properties"]["kind"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(10),
            "kind must expose its exact bounded vocabulary"
        );
        assert_eq!(
            spec.input_schema["properties"]["scope"]["enum"]
                .as_array()
                .map(Vec::len),
            Some(5),
            "scope must expose its exact bounded vocabulary"
        );
        let ops = spec.input_schema["properties"]["op"]["enum"]
            .as_array()
            .expect("op enum");
        assert!(
            !ops.iter().any(|op| op.as_str() == Some("gc_hint")),
            "gc_hint is not a model-facing op: {ops:?}"
        );
        assert!(
            !ops.iter().any(|op| op.as_str() == Some("collect")),
            "collect is not a model-facing op: {ops:?}"
        );
        assert!(
            spec.description.contains("context://run/"),
            "schema names the catalog uri the mutation ops consume: {}",
            spec.description
        );
    }

    #[tokio::test]
    async fn model_surface_uses_one_artifact_paging_primitive() {
        let dispatcher = dispatcher().await;
        for name in ["fs.list", "search.grep", "code.symbols"] {
            let spec = dispatcher.inspect_tool(name).expect("builtin tool spec");
            assert!(
                spec.input_schema["properties"].get("cursor").is_none(),
                "{name} must not invite the model to invent an opaque cursor"
            );
            assert!(
                spec.description.contains("artifact.read"),
                "{name} must route overflow through the shared artifact reader"
            );
        }
        let artifact = dispatcher
            .inspect_tool("artifact.read")
            .expect("artifact.read spec");
        assert!(
            artifact.input_schema["properties"]
                .get("reference")
                .is_some()
        );
        assert!(
            artifact.input_schema["properties"]
                .get("start_line")
                .is_some()
        );
    }

    #[tokio::test]
    async fn context_manage_attaches_typed_directives() {
        let dispatcher = dispatcher().await;
        let names = surface(&dispatcher);
        assert!(
            !names.contains(&"context.manage".to_string()),
            "item 24: context.manage starts catalog-only: {names:?}"
        );

        let item_id = "00000000-0000-0000-0000-000000000000";
        let item_uri = format!("context://run/{item_id}");

        // The directive ops return a `RuntimeDirective` (a distinct
        // `ToolOutcome` variant), not a field on the output.
        let directive = |outcome: ToolOutcome| match outcome {
            ToolOutcome::RuntimeDirective { directive, .. } => directive,
            other => panic!("context tools must return a directive, got {other:?}"),
        };

        let output = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "tag", "item_id": item_uri, "tag": "urgent"}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::Tag { ref tag, .. })
                if tag == "urgent"
        ));

        let output = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "lease", "item_id": item_id, "turns": 3}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::Lease { turns: 3, .. })
        ));

        // Bad arguments are rejected like any other tool. gc_hint / collect
        // are not model-facing ops: fail closed, no tutorial.
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "gc_hint"})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("args") || error.to_string().contains("unknown"),
            "{error}"
        );
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "collect"})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("args") || error.to_string().contains("unknown"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn context_manage_query_ops_emit_engine_queries() {
        let dispatcher = dispatcher().await;

        // The retrieval loop is read-only and engine-serviced: the tool
        // names what it wants (`EngineQuery`), the kernel resolves it.
        let query = |outcome: ToolOutcome| match outcome {
            ToolOutcome::EngineQuery { query, .. } => query,
            other => panic!("context query tools must emit an engine query, got {other:?}"),
        };

        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "search", "query": "AuthService", "limit": 8}),
            ))
            .await
            .unwrap();
        match query(outcome) {
            agent_contracts::EngineQuery::SearchExternal {
                query,
                limit,
                kind,
                scope,
                task_id,
                label,
            } => {
                assert_eq!(query, "AuthService");
                assert_eq!(limit, 8);
                assert!(kind.is_none() && scope.is_none() && task_id.is_none());
                assert!(label.is_none());
            }
            other => panic!("expected SearchExternal, got {other:?}"),
        }

        let item_id = "00000000-0000-0000-0000-000000000000";
        let item_uri = format!("context://run/{item_id}");
        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "inspect", "item_id": item_uri}),
            ))
            .await
            .unwrap();
        match query(outcome) {
            agent_contracts::EngineQuery::InspectExternal { item_id: parsed } => {
                assert_eq!(parsed.to_string(), item_id);
            }
            other => panic!("expected InspectExternal, got {other:?}"),
        }

        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "fetch", "item_id": item_id}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::FetchExternal { .. }
        ));

        // Union-shaped tool arguments are dispatched by `op`: malformed
        // placeholders for fields that fetch does not consume cannot poison
        // an otherwise valid fetch.
        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({
                    "op": "fetch",
                    "item_id": item_id,
                    "kind": "",
                    "scope": "",
                    "task_id": "",
                    "fact": "",
                    "label": ""
                }),
            ))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::FetchExternal { .. }
        ));

        // The schema advertises canonical names, while the executor also
        // accepts their stable lowercase wire spellings.
        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({
                    "op": "search",
                    "query": "AuthService",
                    "kind": "constraint",
                    "scope": "task"
                }),
            ))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::SearchExternal {
                kind: Some(agent_contracts::ContextKind::Constraint),
                scope: Some(agent_contracts::ContextScope::Task),
                ..
            }
        ));

        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "search", "label": "decision"}),
            ))
            .await
            .unwrap();
        match query(outcome) {
            agent_contracts::EngineQuery::SearchExternal { query, label, .. } => {
                assert_eq!(query, "");
                assert_eq!(label.as_deref(), Some("decision"));
            }
            other => panic!("expected SearchExternal, got {other:?}"),
        }

        // Bad arguments are rejected like any other tool.
        let error = dispatcher
            .execute(request("context.manage", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("args"), "{error}");
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "search"})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing 'query'"), "{error}");
        let error = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "search", "query": "AuthService", "kind": "Task"}),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid 'kind'"), "{error}");
        let error = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "fetch", "item_id": "", "kind": "Constraint"}),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing 'item_id'"), "{error}");
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "gc_hint"})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("args") || error.to_string().contains("unknown"),
            "{error}"
        );
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "collect"})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("args") || error.to_string().contains("unknown"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn load_and_unload_change_the_model_surface() {
        let dispatcher = dispatcher().await;
        assert!(!surface(&dispatcher).contains(&"edit.replace".to_string()));
        // task.complete ships on the production surface (closure discovery);
        // an explicit load stays idempotent and unload still cools it.
        assert!(surface(&dispatcher).contains(&"task.complete".to_string()));

        dispatcher.load("edit.replace").unwrap();
        assert!(surface(&dispatcher).contains(&"edit.replace".to_string()));

        dispatcher.load("task.complete").unwrap();
        assert!(surface(&dispatcher).contains(&"task.complete".to_string()));

        dispatcher.unload("edit.replace").unwrap();
        assert!(!surface(&dispatcher).contains(&"edit.replace".to_string()));
        // task.complete is always-loaded now: unload refuses, exactly like
        // the other core tools.
        let unload = dispatcher.unload("task.complete");
        assert!(
            unload.is_err(),
            "an always-loaded closure surface cannot be unloaded"
        );
        assert!(surface(&dispatcher).contains(&"task.complete".to_string()));

        // Core tools cannot be unloaded.
        let core = dispatcher.unload("fs.read");
        assert!(core.is_err(), "core tools must stay loaded");
        assert!(dispatcher.unload("git.status").is_err());
    }

    #[tokio::test]
    async fn round_omission_classification_preserves_core_and_controls() {
        let dispatcher = dispatcher().await;
        assert!(!dispatcher.may_omit_from_round("fs.read"));
        assert!(
            dispatcher.may_omit_from_round(CONTEXT_MANAGE),
            "item 24: context.manage follows catalog lifecycle, not fail-closed omission"
        );
        assert!(!dispatcher.may_omit_from_round(CAPABILITY_MANAGE));
        assert!(!dispatcher.may_omit_from_round("unknown.tool"));
        assert!(!dispatcher.may_omit_from_round("git.status"));
        assert!(dispatcher.may_omit_from_round("edit.replace"));
        assert!(
            !dispatcher.may_omit_from_round("task.complete"),
            "closure discovery is always-surface and fail-closed omission"
        );
    }

    #[tokio::test]
    async fn surface_specs_and_generation_are_one_catalog_revision() {
        let dispatcher = Arc::new(dispatcher().await);
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_dispatcher = dispatcher.clone();
        let writer_finished = finished.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..2_000 {
                writer_dispatcher.load("edit.replace").unwrap();
                writer_dispatcher.unload("edit.replace").unwrap();
            }
            writer_finished.store(true, Ordering::Release);
        });

        while !finished.load(Ordering::Acquire) {
            let snapshot = dispatcher.surface();
            let loaded = snapshot
                .specs
                .iter()
                .any(|spec| spec.name == "edit.replace");
            assert_eq!(
                loaded,
                snapshot.generation % 2 == 1,
                "specs do not describe generation {}",
                snapshot.generation
            );
            assert_eq!(
                snapshot.source_revisions.builtin_catalog_generation,
                snapshot.generation
            );
        }
        writer.join().unwrap();
    }

    #[tokio::test]
    async fn idle_tools_cool_and_unload() {
        let (workspace, _dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 2,
                warm_to_unload_ticks: 4,
                // 旧语义：永远视为超压，闲置即冷却。
                surface_soft_high_bytes: 0,
                surface_low_watermark_bytes: 0,
            },
        );
        dispatcher.load_for_lease("fs.write").unwrap();
        assert!(surface(&dispatcher).contains(&"fs.write".to_string()));
        // specs() is pure: reading the surface must not age the lifecycle.
        for _ in 0..3 {
            let _ = surface(&dispatcher);
        }
        assert!(
            surface(&dispatcher).contains(&"fs.write".to_string()),
            "specs() must never mutate the tool lifecycle"
        );
        // Only an explicit gc() at the runtime safe point ages the catalog.
        for _ in 0..4 {
            dispatcher.gc(&[]);
        }
        assert!(
            !surface(&dispatcher).contains(&"fs.write".to_string()),
            "an idle tool must leave the surface after the GC thresholds"
        );
    }

    #[tokio::test]
    async fn no_surface_pressure_keeps_idle_tools_loaded() {
        let (workspace, _dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 2,
                warm_to_unload_ticks: 4,
                // 水位远超全部 schema：无表面压力，闲置工具留在表面。
                surface_soft_high_bytes: 1_000_000,
                surface_low_watermark_bytes: 500_000,
            },
        );
        dispatcher.load_for_lease("fs.write").unwrap();
        for _ in 0..10 {
            dispatcher.gc(&[]);
        }
        assert!(
            surface(&dispatcher).contains(&"fs.write".to_string()),
            "低于高水位不得冷却可选工具：保留成本低于一次重载轮"
        );
    }

    #[tokio::test]
    async fn decision_lease_reconcile_cools_only_unrooted_optionals() {
        let (workspace, _dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 100,
                warm_to_unload_ticks: 200,
                // Prove that lease reconciliation is independent of byte
                // pressure and the idle clock.
                surface_soft_high_bytes: 1_000_000,
                surface_low_watermark_bytes: 500_000,
            },
        );
        dispatcher.load_for_lease("fs.write").unwrap();
        dispatcher.load_for_lease("git.status").unwrap();

        let report = dispatcher.reconcile_leases(&["fs.write".to_string()]);
        assert_eq!(report.examined_loaded_optional, 2);
        assert_eq!(report.retained_by_root, 1);
        assert_eq!(report.released_to_warm, 1);
        assert_eq!(report.released_tools, vec!["git.status"]);
        let rows = dispatcher.catalog();
        let state = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .map(|row| row.state)
                .unwrap()
        };
        assert_eq!(state("fs.read"), ToolLifecycle::Loaded);
        assert_eq!(state("fs.write"), ToolLifecycle::Loaded);
        assert_eq!(state("git.status"), ToolLifecycle::Warm);

        dispatcher.load_for_lease("git.status").unwrap();
        assert!(
            dispatcher
                .specs()
                .iter()
                .any(|spec| spec.name == "git.status"),
            "a released lease must remain exactly reloadable"
        );
    }

    #[tokio::test]
    async fn host_load_source_survives_reconcile_and_pressure_until_unload() {
        let (workspace, _dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 1,
                warm_to_unload_ticks: 2,
                surface_soft_high_bytes: 0,
                surface_low_watermark_bytes: 0,
            },
        );
        ToolDispatcher::load_tool(&dispatcher, "git.status").unwrap();
        dispatcher.load_for_lease("fs.write").unwrap();

        let report = dispatcher.reconcile_leases(&[]);
        assert_eq!(report.examined_loaded_optional, 2);
        assert_eq!(report.retained_by_persistent_source, 1);
        assert_eq!(report.released_to_warm, 1);
        for _ in 0..4 {
            dispatcher.gc(&[]);
        }
        let rows = dispatcher.catalog();
        assert_eq!(
            rows.iter()
                .find(|row| row.name == "git.status")
                .map(|row| row.state),
            Some(ToolLifecycle::Loaded)
        );
        ToolDispatcher::unload_tool(&dispatcher, "git.status").unwrap();
        assert_eq!(
            dispatcher
                .catalog()
                .iter()
                .find(|row| row.name == "git.status")
                .map(|row| row.state),
            Some(ToolLifecycle::Unloaded)
        );
    }

    #[tokio::test]
    async fn pressure_cools_oldest_first_to_the_low_watermark() {
        let (_workspace, _dir) = open_workspace().await;
        let legacy = |workspace| {
            BuiltinToolDispatcher::with_config(
                workspace,
                ToolLifecycleConfig {
                    always_loaded: vec!["fs.read".into()],
                    idle_to_warm_ticks: 2,
                    warm_to_unload_ticks: 100,
                    surface_soft_high_bytes: 0,
                    surface_low_watermark_bytes: 0,
                },
            )
        };
        // 先量出 gc 所见的全部已加载条目（不含 meta 控制工具）的 schema
        // 总量，再把高水位压到总量之下：恰好必须冷却一个，低水位等于
        // 高水位使冷却在一条后立即停止。
        let measuring = legacy(open_workspace().await.0);
        measuring.load("fs.write").unwrap();
        measuring.load("git.status").unwrap();
        let total: usize = measuring
            .specs()
            .iter()
            .filter(|spec| spec.name != CAPABILITY_MANAGE)
            .map(|spec| serde_json::to_string(spec).unwrap().len())
            .sum();
        drop(measuring);
        let (workspace, _dir) = open_workspace().await;
        let watermark = total - 1;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 2,
                warm_to_unload_ticks: 100,
                surface_soft_high_bytes: watermark,
                surface_low_watermark_bytes: watermark,
            },
        );
        dispatcher.load_for_lease("fs.write").unwrap();
        dispatcher.load_for_lease("git.status").unwrap();
        for _ in 0..4 {
            dispatcher.gc(&[]);
        }
        assert!(
            !surface(&dispatcher).contains(&"fs.write".to_string()),
            "最久未用先冷却"
        );
        assert!(
            surface(&dispatcher).contains(&"git.status".to_string()),
            "到达低水位即停止：较新的工具保留"
        );
        for _ in 0..4 {
            dispatcher.gc(&[]);
        }
        assert!(
            surface(&dispatcher).contains(&"git.status".to_string()),
            "滞回：已到低水位，后续轮不再继续冷却"
        );
    }

    #[tokio::test]
    async fn task_roots_protect_required_tools_from_idle_gc() {
        let (workspace, _dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 2,
                warm_to_unload_ticks: 4,
                // 旧语义：永远视为超压，闲置即冷却。
                surface_soft_high_bytes: 0,
                surface_low_watermark_bytes: 0,
            },
        );
        dispatcher.load_for_lease("fs.write").unwrap();
        dispatcher.load_for_lease("git.status").unwrap();
        assert!(surface(&dispatcher).contains(&"git.status".to_string()));

        // The active task requires fs.write but not git.status. Idle GC
        // must honor the root: fs.write stays on the surface while the
        // unrooted git.status ages out.
        let roots = vec!["fs.write".to_string()];
        for _ in 0..5 {
            dispatcher.gc(&roots);
        }
        assert!(
            surface(&dispatcher).contains(&"fs.write".to_string()),
            "a task-rooted tool must survive idle GC"
        );
        assert!(
            !surface(&dispatcher).contains(&"git.status".to_string()),
            "an unrooted idle tool must still age out"
        );

        // A root only protects the idle path: an explicit unload is still
        // allowed (the round surface then degrades per task demand).
        assert!(
            dispatcher.unload("fs.write").is_ok(),
            "roots must not block explicit unload"
        );
    }

    #[tokio::test]
    async fn control_tools_execute() {
        let dispatcher = dispatcher().await;
        let search = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "query": "git"}),
            ))
            .await
            .unwrap();
        let search = value(search);
        assert!(search.ok);
        assert!(search.model_content.contains("git.status"));

        let inspect = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "inspect", "name": "fs.read"}),
            ))
            .await
            .unwrap();
        let inspect = value(inspect);
        assert!(inspect.ok);
        assert!(inspect.model_content.contains("fs.read"));

        let load = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "load", "name": "edit.replace"}),
            ))
            .await
            .unwrap();
        let load = value(load);
        assert!(load.ok);
        assert!(surface(&dispatcher).contains(&"edit.replace".to_string()));
        let report = dispatcher.reconcile_leases(&[]);
        assert_eq!(report.retained_by_persistent_source, 0);
        assert_eq!(report.released_to_warm, 1);
        assert!(
            !surface(&dispatcher).contains(&"edit.replace".to_string()),
            "model capability.manage load must remain a transient runtime lease"
        );

        // Unknown ops are rejected.
        let bad = dispatcher
            .execute(request(CAPABILITY_MANAGE, json!({"op": "explode"})))
            .await
            .unwrap_err();
        assert!(bad.to_string().contains("unknown op"), "{bad}");
    }

    #[tokio::test]
    async fn capability_search_matches_description_case_insensitively_and_does_not_load() {
        let dispatcher = dispatcher().await;
        let before = surface(&dispatcher);
        let search = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "query": "status --short"}),
            ))
            .await
            .unwrap();
        let search = value(search);
        assert!(search.ok);
        assert!(
            search.model_content.contains("git.status"),
            "descriptor search must match description text: {}",
            search.model_content
        );
        assert_eq!(
            search.metadata["op"], "search",
            "search results must be tagged for transient disposition"
        );
        assert!(search.metadata["descriptors"].is_array());
        assert_eq!(
            surface(&dispatcher),
            before,
            "search must not load or admit a tool"
        );

        let by_name = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "query": "GIT.STATUS"}),
            ))
            .await
            .unwrap();
        let by_name = value(by_name);
        assert!(by_name.model_content.contains("git.status"));

        let natural = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "query": "patch edit file"}),
            ))
            .await
            .unwrap();
        let natural = value(natural);
        assert!(
            natural.model_content.contains("edit.patch"),
            "token-OR search must hit edit.patch: {}",
            natural.model_content
        );

        let by_role = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "role": "mutate"}),
            ))
            .await
            .unwrap();
        let by_role = value(by_role);
        assert!(
            by_role.model_content.contains("edit.patch"),
            "role=mutate must hit edit.patch: {}",
            by_role.model_content
        );
        assert!(
            !by_role.model_content.contains("git.status"),
            "role=mutate must not leak InspectDiff: {}",
            by_role.model_content
        );
        assert!(
            !by_role.model_content.contains("shell.exec"),
            "role=mutate must not leak EscapeHatch: {}",
            by_role.model_content
        );

        let bad_role = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "role": "planner"}),
            ))
            .await;
        assert!(
            matches!(bad_role, Err(AgentError::InvalidRequest(_))),
            "unknown role must refuse: {bad_role:?}"
        );

        let unknown = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "inspect", "name": "no.such.tool"}),
            ))
            .await
            .unwrap();
        let unknown = value(unknown);
        assert!(!unknown.ok);
        assert_eq!(unknown.metadata["miss"], "not_found");
        assert_eq!(unknown.metadata["op"], "inspect");
    }

    #[tokio::test]
    async fn capability_search_pages_and_spills_large_catalogs() {
        let dispatcher = dispatcher().await;
        // A small catalog fits the default page: no artifact, rows in line.
        let small = dispatcher
            .execute(request(CAPABILITY_MANAGE, json!({"op": "search"})))
            .await
            .unwrap();
        let small = value(small);
        assert!(small.ok);
        assert!(small.artifact_ref.is_none(), "small catalogs stay inline");
        assert!(small.model_content.contains("fs.read"));

        // A tiny page forces the spill: the full listing goes to an
        // artifact and only the bounded page reaches the model.
        let paged = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "limit": 2}),
            ))
            .await
            .unwrap();
        let paged = value(paged);
        assert!(paged.ok);
        assert!(
            paged.artifact_ref.is_some(),
            "a catalog larger than the page must spill to an artifact"
        );
        assert!(
            paged.model_content.lines().count() <= 2,
            "the model must only see the bounded page, got: {}",
            paged.model_content
        );
        assert_eq!(
            paged.metadata["has_more"], true,
            "more rows must be reported"
        );

        // The cursor pages past the first two rows: the second page must
        // start where the first ended and must not repeat it.
        let cursor = paged
            .model_content
            .lines()
            .last()
            .map(|line| line.split('\t').nth(1).unwrap_or("").to_string())
            .unwrap_or_default();
        assert!(!cursor.is_empty(), "a cursor must be extractable");
        let next = dispatcher
            .execute(request(
                CAPABILITY_MANAGE,
                json!({"op": "search", "limit": 2, "cursor": cursor}),
            ))
            .await
            .unwrap();
        let next = value(next);
        assert!(next.ok);
        assert!(
            !next.model_content.contains(&cursor),
            "the first page must not repeat; got: {}",
            next.model_content
        );
    }

    /// Token benchmark for the merged control surface: the always-visible
    /// schemas must be measurably cheaper than the old dozen
    /// single-purpose meta-tools. The numbers are the evidence for keeping
    /// the merge — "less context" is the design goal, measured, not assumed.
    #[test]
    fn merged_control_surface_costs_fewer_schema_tokens() {
        use agent_contracts::tokens::approx_tokens;

        let old_surface: Vec<ToolSpec> = vec![
            ToolSpec {
                name: "fs.list".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "fs.read".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::ReadResource],
            },
            ToolSpec {
                name: "search.grep".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "context.gc_hint".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.tag".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.lease".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.collect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.search".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.inspect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "context.fetch".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "capability.search".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "capability.inspect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "capability.load".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "capability.unload".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
        ];
        let merged_surface: Vec<ToolSpec> = vec![
            ToolSpec {
                name: "fs.list".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "fs.read".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::ReadResource],
            },
            ToolSpec {
                name: "search.grep".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "context.manage".into(),
                description: "x".repeat(120),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
            ToolSpec {
                name: "capability.manage".into(),
                description: "x".repeat(120),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: vec![ToolSemanticRole::Search],
            },
        ];
        let old_tokens: usize = old_surface
            .iter()
            .map(|spec| approx_tokens(&serde_json::to_string(spec).unwrap_or_default()))
            .sum();
        let merged_tokens: usize = merged_surface
            .iter()
            .map(|spec| approx_tokens(&serde_json::to_string(spec).unwrap_or_default()))
            .sum();
        assert!(
            merged_tokens < old_tokens,
            "the merged control surface must cost fewer schema tokens: merged {merged_tokens} vs separate {old_tokens}"
        );
        assert!(
            merged_tokens * 2 < old_tokens,
            "the merge must be a decisive win (merged {merged_tokens}, separate {old_tokens})"
        );
    }

    #[tokio::test]
    async fn dispatcher_refuses_process_session_without_effect_identity() {
        let dispatcher = dispatcher().await;
        for arguments in [
            json!({"action": "start", "argv": ["echo", "hi"]}),
            json!({"action": "poll", "session_id": "s1"}),
            json!({"action": "stop", "session_id": "s1"}),
        ] {
            let error = dispatcher
                .execute(request("process.session", arguments.clone()))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(
                    "non-transactional process tools require a Core-issued effect context"
                ),
                "dispatch must refuse {arguments} without identity: {error}"
            );
        }
    }

    #[tokio::test]
    async fn dispatcher_drop_kills_live_session_trees() {
        let (workspace, dir) = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::new(workspace);
        let (argv, child_pidfile, heir_pidfile) =
            crate::tools::test_procs::tree_pidfile_argv(dir.path());
        let arguments = json!({"action": "start", "argv": argv});
        let mut request = request("process.session", arguments.clone());
        request.effect_context = Some(crate::tools::test_process_effect_context(
            request.run_id,
            &request.call.id,
            "process.session",
            &arguments,
        ));
        let outcome = dispatcher.execute(request).await.unwrap();
        match outcome {
            ToolOutcome::Value(output) => assert!(output.ok, "{}", output.summary),
            other => panic!("session start must return a plain value: {other:?}"),
        }
        crate::tools::test_procs::wait_for_path(&child_pidfile).await;
        let child_pid = crate::tools::test_procs::read_pid(&child_pidfile);
        let heir_pid: Option<u32> = std::fs::read_to_string(&heir_pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        drop(dispatcher);
        let mut tracked = vec![child_pid];
        if let Some(heir_pid) = heir_pid {
            tracked.push(heir_pid);
        }
        crate::tools::test_procs::wait_for_all_dead(&tracked, "the dropped dispatcher's sessions");
    }

    #[tokio::test]
    async fn dispatcher_refuses_effect_identity_on_readonly_git() {
        let dispatcher = dispatcher().await;
        let mut request = request("git.status", json!({}));
        request.effect_context = Some(crate::tools::test_process_effect_context(
            request.run_id,
            &request.call.id,
            "git.status",
            &request.call.arguments,
        ));
        let error = dispatcher.execute(request).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must not receive a Core-issued effect context"),
            "a ReadOnly git spawn must not be laundered through a process identity: {error}"
        );
    }

    #[tokio::test]
    async fn builtin_attribution_copies_only_trusted_resource_identities() {
        let dispatcher = dispatcher().await;
        let read = ToolCall {
            id: "read-1".into(),
            name: "fs.read".into(),
            arguments: json!({"path": r"src\lib.rs", "offset": 10, "needle": "secret"}),
        };
        let attribution = dispatcher.execution_attribution(&read);
        assert_eq!(attribution.purpose, ToolExecutionPurpose::Observe);
        assert_eq!(attribution.targets, vec!["src/lib.rs"]);
        assert_eq!(attribution.verification_reuse, VerificationReuse::None);

        let shell = ToolCall {
            id: "shell-1".into(),
            name: "shell.exec".into(),
            arguments: json!({"command": "cargo test"}),
        };
        let shell_attribution = dispatcher.execution_attribution(&shell);
        assert_eq!(shell_attribution.purpose, ToolExecutionPurpose::Opaque);
        assert!(shell_attribution.targets.is_empty());
        assert!(!shell_attribution.reusable_verification());
    }
}

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
    ToolExecutionRequest, ToolOutcome, ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
    ToolSurfaceSnapshot, reject_staged_effect_for_process_tool, search_tool_catalog_filtered,
};
use agent_workspace::Workspace;
use serde::Deserialize;
use serde_json::json;

use crate::tools::{
    ArtifactReadTool, CodeDiagnosticsTool, CodeSymbolsTool, ContextManageTool, EditPatchTool,
    EditReplaceTool, FsListTool, FsReadTool, FsWriteTool, GitDiffTool, GitStatusTool,
    ProcessRunTool, ProcessSession, ProcessSessionTool, SearchGrepTool, ShellExecTool,
    TaskCompleteTool, Tool,
};

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
        // Production always-loaded surface. Git / shell / `fs.write` /
        // `edit.replace` / `context.manage` stay in the catalog and load
        // through `capability.manage` (or runtime NeedEvidence).
        // `edit.patch` is the single canonical mutation primitive: one
        // extra discovery round costs more than keeping its compact schema
        // on the core coding surface. Scripted eval fixtures,
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
                // Canonical revision-aware mutation. Not fs.write /
                // edit.replace / git / shell / process.
                "edit.patch".into(),
                // Completion is a task-level control: the model can always
                // propose a structured outcome.
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
}

pub struct BuiltinToolDispatcher {
    catalog: RwLock<HashMap<String, ToolEntry>>,
    config: ToolLifecycleConfig,
    /// Kept for `capability.search` artifact spill: a catalog that does not
    /// fit the model-facing page writes its full listing here.
    workspace: Workspace,
    tick: AtomicU64,
    /// Bumped on every lifecycle change (load/unload/gc transitions), so a
    /// `ToolSurfaceSnapshot` is auditably identifiable.
    generation: AtomicU64,
}

impl BuiltinToolDispatcher {
    pub fn new(workspace: Workspace) -> Self {
        Self::with_config(workspace, ToolLifecycleConfig::default())
    }

    pub fn with_config(workspace: Workspace, config: ToolLifecycleConfig) -> Self {
        // One session registry per dispatcher, shared by every
        // `process.session` tool instance.
        let sessions: std::sync::Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, ProcessSession>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FsListTool::new(workspace.clone())),
            Arc::new(FsReadTool::new(workspace.clone())),
            Arc::new(ArtifactReadTool::new(workspace.clone())),
            Arc::new(FsWriteTool::new(workspace.clone())),
            Arc::new(SearchGrepTool::new(workspace.clone())),
            Arc::new(EditReplaceTool::new(workspace.clone())),
            Arc::new(EditPatchTool::new(workspace.clone())),
            Arc::new(GitStatusTool::new(workspace.clone())),
            Arc::new(GitDiffTool::new(workspace.clone())),
            Arc::new(ShellExecTool::new(workspace.clone())),
            Arc::new(ProcessRunTool::new(workspace.clone())),
            Arc::new(ProcessSessionTool::new(workspace.clone(), sessions)),
            // Local symbol/diagnostic navigation: catalog-optional
            // first-party tools loaded on demand when a task needs precise
            // navigation (pure local scans, no embeddings/vector storage).
            Arc::new(CodeSymbolsTool::new(workspace.clone())),
            Arc::new(CodeDiagnosticsTool::new(workspace.clone())),
            Arc::new(ContextManageTool::new()),
            Arc::new(TaskCompleteTool::new()),
        ];
        let catalog = tools
            .into_iter()
            .map(|tool| {
                let spec = tool.spec();
                let state = if config.always_loaded.contains(&spec.name) {
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
                    },
                )
            })
            .collect();
        Self {
            catalog: RwLock::new(catalog),
            config,
            workspace,
            tick: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// Load a catalog tool into the active set (or re-load a warm/unloaded
    /// one) so its schema appears on the next model request.
    pub fn load(&self, name: &str) -> AgentResult<()> {
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
        if self.config.always_loaded.iter().any(|core| core == name) {
            return Err(AgentError::InvalidRequest(format!(
                "core tool '{name}' cannot be unloaded"
            )));
        }
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let entry = catalog.get_mut(name).ok_or_else(|| {
            AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
        })?;
        entry.state = ToolLifecycle::Unloaded;
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
                    if self.config.always_loaded.iter().any(|core| core == name)
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
            if self.config.always_loaded.iter().any(|core| core == name)
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
            && !self.config.always_loaded.iter().any(|core| core == name)
    }

    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        Self::catalog(self)
    }

    fn load_tool(&self, name: &str) -> AgentResult<()> {
        self.load(name)
    }

    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        self.unload(name)
    }

    fn inspect_tool(&self, name: &str) -> Option<ToolSpec> {
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        catalog.get(name).map(|entry| entry.tool.spec())
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
        })
    }

    async fn run_load(
        &self,
        request: ToolExecutionRequest,
        name: String,
    ) -> AgentResult<ToolOutput> {
        self.load(&name)?;
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
        self.unload(&name)?;
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
mod tests {
    use super::*;
    use agent_contracts::CONTEXT_MANAGE;
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
            !names.contains(&"context.gc_hint".to_string()),
            "the meta-tools must be merged into context.manage: {names:?}"
        );
        assert!(
            !names.contains(&"capability.search".to_string()),
            "the catalog control tools must be merged into capability.manage: {names:?}"
        );
        assert!(
            !names.contains(&"git.status".to_string()),
            "git tools must not be loaded by default: {names:?}"
        );
        assert!(
            !names.contains(&"fs.write".to_string()),
            "write tools must be loaded on demand: {names:?}"
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
        assert!(!surface(&dispatcher).contains(&"git.status".to_string()));

        dispatcher.load("git.status").unwrap();
        assert!(surface(&dispatcher).contains(&"git.status".to_string()));

        dispatcher.unload("git.status").unwrap();
        assert!(!surface(&dispatcher).contains(&"git.status".to_string()));

        // Core tools cannot be unloaded.
        let core = dispatcher.unload("fs.read");
        assert!(core.is_err(), "core tools must stay loaded");
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
        assert!(dispatcher.may_omit_from_round("git.status"));
    }

    #[tokio::test]
    async fn surface_specs_and_generation_are_one_catalog_revision() {
        let dispatcher = Arc::new(dispatcher().await);
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_dispatcher = dispatcher.clone();
        let writer_finished = finished.clone();
        let writer = std::thread::spawn(move || {
            for _ in 0..2_000 {
                writer_dispatcher.load("git.status").unwrap();
                writer_dispatcher.unload("git.status").unwrap();
            }
            writer_finished.store(true, Ordering::Release);
        });

        while !finished.load(Ordering::Acquire) {
            let snapshot = dispatcher.surface();
            let loaded = snapshot.specs.iter().any(|spec| spec.name == "git.status");
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
        dispatcher.load("fs.write").unwrap();
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
        dispatcher.load("fs.write").unwrap();
        for _ in 0..10 {
            dispatcher.gc(&[]);
        }
        assert!(
            surface(&dispatcher).contains(&"fs.write".to_string()),
            "低于高水位不得冷却可选工具：保留成本低于一次重载轮"
        );
    }

    #[tokio::test]
    async fn pressure_cools_oldest_first_to_the_low_watermark() {
        let (workspace, _dir) = open_workspace().await;
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
        dispatcher.load("fs.write").unwrap();
        dispatcher.load("git.status").unwrap();
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
        dispatcher.load("fs.write").unwrap();
        dispatcher.load("git.status").unwrap();
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
                json!({"op": "load", "name": "git.status"}),
            ))
            .await
            .unwrap();
        let load = value(load);
        assert!(load.ok);
        assert!(surface(&dispatcher).contains(&"git.status".to_string()));

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
}

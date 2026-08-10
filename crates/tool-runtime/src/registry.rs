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
    AgentError, AgentResult, ToolCatalogEntry, ToolDispatcher, ToolExecutionRequest, ToolOutcome,
    ToolOutput, ToolRisk, ToolSpec, ToolSurfaceSnapshot,
};
use agent_workspace::Workspace;
use serde::Deserialize;
use serde_json::json;

use crate::tools::{
    ContextManageTool, EditReplaceTool, FsListTool, FsReadTool, FsWriteTool, GitDiffTool,
    GitStatusTool, SearchGrepTool, ShellExecTool, Tool,
};

/// Control tools are now defined by the unified catalog contract.
pub use agent_contracts::{CAPABILITY_MANAGE, CONTEXT_MANAGE, ToolLifecycle};

/// Tuning knobs for the catalog lifecycle.
#[derive(Debug, Clone)]
pub struct ToolLifecycleConfig {
    /// Tools that stay loaded for the whole run (the model always sees them).
    pub always_loaded: Vec<String>,
    /// Idle ticks before a loaded tool cools to Warm.
    pub idle_to_warm_ticks: usize,
    /// Idle ticks before a warm tool is unloaded from the model surface.
    pub warm_to_unload_ticks: usize,
}

impl Default for ToolLifecycleConfig {
    fn default() -> Self {
        Self {
            always_loaded: vec![
                "fs.list".into(),
                "fs.read".into(),
                "search.grep".into(),
                // The merged control surface: one `context.manage` (gc hints,
                // tags, leases, manual collect, and the on-demand retrieval
                // loop over externalized refs) and one `capability.manage`
                // (catalog search/inspect/load/unload). A dozen
                // single-purpose meta-tools would cost more model input than
                // the runtime control they provide.
                CONTEXT_MANAGE.into(),
                CAPABILITY_MANAGE.into(),
            ],
            idle_to_warm_ticks: 8,
            warm_to_unload_ticks: 24,
        }
    }
}

struct ToolEntry {
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
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(FsListTool::new(workspace.clone())),
            Arc::new(FsReadTool::new(workspace.clone())),
            Arc::new(FsWriteTool::new(workspace.clone())),
            Arc::new(SearchGrepTool::new(workspace.clone())),
            Arc::new(EditReplaceTool::new(workspace.clone())),
            Arc::new(GitStatusTool::new(workspace.clone())),
            Arc::new(GitDiffTool::new(workspace.clone())),
            Arc::new(ShellExecTool::new(workspace.clone())),
            Arc::new(ContextManageTool::new()),
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
        let tick = self.tick_now();
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
            .map(|(name, entry)| ToolCatalogEntry {
                name: name.clone(),
                state: entry.state,
                owner: "builtin".to_string(),
                description: entry.tool.spec().description.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    fn tick_now(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Age transitions at an explicit runtime safe point: idle tools cool
    /// Loaded -> Warm and then Warm -> Unloaded, so the model surface
    /// tracks recent use. Core tools never age out. Called once per model
    /// round by the runtime — never implicitly from `specs()`, which must
    /// stay pure so budget, prompt and tool-call validation all observe one
    /// stable surface per round.
    pub fn gc(&self) {
        let tick = self.tick_now();
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        let mut changed = false;
        for (name, entry) in catalog.iter_mut() {
            if self.config.always_loaded.iter().any(|core| core == name) {
                continue;
            }
            let idle = tick.saturating_sub(entry.last_used_tick);
            match entry.state {
                ToolLifecycle::Loaded if idle >= self.config.idle_to_warm_ticks as u64 => {
                    entry.state = ToolLifecycle::Warm;
                    changed = true;
                }
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
                description: "Manage the tool catalog in one call. ops: search (list known tools with lifecycle state and owner), inspect (one tool's schema and state), load (put a tool on the model surface, e.g. git.status), unload (take a tool off the surface; core tools cannot be unloaded).".into(),
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
                risk: ToolRisk::ReadOnly,
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
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
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

#[async_trait::async_trait]
impl ToolDispatcher for BuiltinToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        // Pure read: lifecycle aging happens only at the explicit runtime
        // safe point (`gc()`), never here, so one model round sees a stable
        // tool surface for budget, prompt and validation alike.
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        Self::surface_specs(&catalog)
    }

    fn gc(&self) {
        // The explicit lifecycle safe point the runtime calls once per
        // model round; delegates to the inherent method so callers on the
        // concrete type and through the trait observe the same aging.
        Self::gc(self);
    }

    fn snapshot(&self) -> ToolSurfaceSnapshot {
        self.surface()
    }

    fn may_omit_from_round(&self, name: &str) -> bool {
        // The catalog configuration is the authority for builtin core
        // membership. Runtime controls are fail-closed even when a custom
        // configuration accidentally leaves one out of `always_loaded`;
        // unknown names are also fail-closed rather than guessed optional.
        if matches!(name, CAPABILITY_MANAGE | CONTEXT_MANAGE) {
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
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_MANAGE => self.run_manage(request).await.map(ToolOutcome::Value),
            _ => {
                let tick = self.tick_now();
                let tool = {
                    let mut catalog = self.catalog.write().expect("tool catalog poisoned");
                    let entry = catalog.get_mut(&name).ok_or_else(|| {
                        AgentError::Tool(format!("unknown tool: {name} (see capability.search)"))
                    })?;
                    entry.state = ToolLifecycle::Active;
                    entry.last_used_tick = tick;
                    entry.tool.clone()
                };
                let output = tool
                    .execute(
                        request.run_id,
                        &request.call.id,
                        request.call.arguments,
                        request.cancel,
                    )
                    .await;
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
        let mut entries = self.catalog();
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
        // large tool catalog cannot itself become context pollution.
        let artifact_ref = if total > page_size {
            let all: String = entries
                .iter()
                .map(|entry| format!("{}\t{}", entry.state.as_str(), entry.name))
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
        let lines: Vec<String> = page
            .iter()
            .map(|entry| format!("{}\t{}", entry.state.as_str(), entry.name))
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
                "total": total,
                "active": active,
                "returned": page.len(),
                "has_more": has_more,
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
            metadata: json!({"name": spec.name, "owner": "builtin", "state": state}),
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

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Open a throwaway workspace. The catalog only touches the disk on real
    /// tool execution, which these tests never trigger.
    async fn open_workspace() -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        Workspace::open(dir.path()).await.unwrap()
    }

    async fn dispatcher() -> BuiltinToolDispatcher {
        BuiltinToolDispatcher::new(open_workspace().await)
    }

    fn request(name: &str, arguments: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id: agent_contracts::RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments,
            },
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
        assert!(names.contains(&"context.manage".to_string()));
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
    async fn context_manage_attaches_typed_directives() {
        let dispatcher = dispatcher().await;
        let names = surface(&dispatcher);
        assert!(
            names.contains(&"context.manage".to_string()),
            "context.manage must be on the default surface: {names:?}"
        );

        let item_id = "00000000-0000-0000-0000-000000000000";

        // The directive ops return a `RuntimeDirective` (a distinct
        // `ToolOutcome` variant), not a field on the output.
        let directive = |outcome: ToolOutcome| match outcome {
            ToolOutcome::RuntimeDirective { directive, .. } => directive,
            other => panic!("context tools must return a directive, got {other:?}"),
        };

        let output = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "gc_hint", "item_id": item_id, "keep": true}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::GcHint {
                keep_alive: true,
                ..
            })
        ));

        let output = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "tag", "item_id": item_id, "tag": "urgent"}),
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

        let output = dispatcher
            .execute(request("context.manage", json!({"op": "collect"})))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::Collect)
        ));

        // Bad arguments are rejected like any other tool.
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "gc_hint"})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing"), "{error}");
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
            } => {
                assert_eq!(query, "AuthService");
                assert_eq!(limit, 8);
                assert!(kind.is_none() && scope.is_none() && task_id.is_none());
            }
            other => panic!("expected SearchExternal, got {other:?}"),
        }

        let item_id = "00000000-0000-0000-0000-000000000000";
        let outcome = dispatcher
            .execute(request(
                "context.manage",
                json!({"op": "inspect", "item_id": item_id}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::InspectExternal { .. }
        ));

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

        // Bad arguments are rejected like any other tool.
        let error = dispatcher
            .execute(request("context.manage", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("args"), "{error}");
        let error = dispatcher
            .execute(request("context.manage", json!({"op": "gc_hint"})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing"), "{error}");
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
        assert!(!dispatcher.may_omit_from_round(CONTEXT_MANAGE));
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
        let workspace = open_workspace().await;
        let dispatcher = BuiltinToolDispatcher::with_config(
            workspace,
            ToolLifecycleConfig {
                always_loaded: vec!["fs.read".into()],
                idle_to_warm_ticks: 2,
                warm_to_unload_ticks: 4,
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
            dispatcher.gc();
        }
        assert!(
            !surface(&dispatcher).contains(&"fs.write".to_string()),
            "an idle tool must leave the surface after the GC thresholds"
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
            },
            ToolSpec {
                name: "fs.read".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "search.grep".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.gc_hint".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.tag".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.lease".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.collect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.search".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.inspect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.fetch".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "capability.search".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "capability.inspect".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "capability.load".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "capability.unload".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
        ];
        let merged_surface: Vec<ToolSpec> = vec![
            ToolSpec {
                name: "fs.list".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "fs.read".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "search.grep".into(),
                description: "x".repeat(60),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "context.manage".into(),
                description: "x".repeat(120),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: "capability.manage".into(),
                description: "x".repeat(120),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
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
}

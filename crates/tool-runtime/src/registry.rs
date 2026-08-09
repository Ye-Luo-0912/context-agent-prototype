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
    ContextDirectiveTool, ContextQueryTool, EditReplaceTool, FsListTool, FsReadTool, FsWriteTool,
    GitDiffTool, GitStatusTool, SearchGrepTool, ShellExecTool, Tool,
};

/// Control tools are now defined by the unified catalog contract.
pub use agent_contracts::{CAPABILITY_LOAD, CAPABILITY_SEARCH, CAPABILITY_UNLOAD, ToolLifecycle};

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
                // The context meta-tools are the model's handle on the
                // context engine (gc hints, tags, leases, manual collect,
                // and the on-demand retrieval loop over externalized refs);
                // they are cheap and always relevant.
                "context.gc_hint".into(),
                "context.tag".into(),
                "context.lease".into(),
                "context.collect".into(),
                "context.search".into(),
                "context.inspect".into(),
                "context.fetch".into(),
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
            Arc::new(ShellExecTool::new(workspace)),
            Arc::new(ContextDirectiveTool::gc_hint()),
            Arc::new(ContextDirectiveTool::tag()),
            Arc::new(ContextDirectiveTool::lease()),
            Arc::new(ContextDirectiveTool::collect()),
            Arc::new(ContextQueryTool::search()),
            Arc::new(ContextQueryTool::inspect()),
            Arc::new(ContextQueryTool::fetch()),
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
        let specs = self.specs();
        let generation = self.generation.load(Ordering::Relaxed);
        ToolSurfaceSnapshot { specs, generation }
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
                name: CAPABILITY_SEARCH.into(),
                description:
                    "List the tools known to the runtime and their lifecycle state (available/loaded/active/warm/unloaded).".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Optional name filter"}
                    }
                }),
                risk: ToolRisk::ReadOnly,
            },
            ToolSpec {
                name: CAPABILITY_LOAD.into(),
                description:
                    "Load a tool into the active set so it appears in the model's tool schemas (e.g. load git.status when the task needs git).".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}}
                }),
                risk: ToolRisk::WorkspaceWrite,
            },
            ToolSpec {
                name: CAPABILITY_UNLOAD.into(),
                description:
                    "Unload a tool from the active set; its schema stops being offered to the model. Core tools cannot be unloaded.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}}
                }),
                risk: ToolRisk::WorkspaceWrite,
            },
        ]
    }
}

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
}

#[async_trait::async_trait]
impl ToolDispatcher for BuiltinToolDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        // Pure read: lifecycle aging happens only at the explicit runtime
        // safe point (`gc()`), never here, so one model round sees a stable
        // tool surface for budget, prompt and validation alike.
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        let mut specs: Vec<ToolSpec> = catalog
            .iter()
            .filter(|(_, entry)| entry.state.in_surface())
            .map(|(_, entry)| entry.tool.spec())
            .collect();
        specs.extend(Self::meta_specs());
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
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
            CAPABILITY_SEARCH => self.run_search(request).await.map(ToolOutcome::Value),
            CAPABILITY_LOAD => self.run_load(request).await.map(ToolOutcome::Value),
            CAPABILITY_UNLOAD => self.run_unload(request).await.map(ToolOutcome::Value),
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
    async fn run_search(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: SearchArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.search args: {e}")))?;
        let mut entries = self.catalog();
        if let Some(query) = args.query.as_deref() {
            entries.retain(|entry| entry.name.contains(query));
        }
        let active = entries
            .iter()
            .filter(|entry| entry.state.in_surface())
            .count();
        let lines: Vec<String> = entries
            .iter()
            .map(|entry| format!("{}\t{}", entry.state.as_str(), entry.name))
            .collect();
        Ok(ToolOutput {
            call_id: request.call.id,
            tool_name: CAPABILITY_SEARCH.into(),
            ok: true,
            summary: format!(
                "{} tools matched ({} in the active set)",
                entries.len(),
                active
            ),
            model_content: lines.join("\n"),
            artifact_ref: None,
            metadata: json!({"total": entries.len(), "active": active}),
        })
    }

    async fn run_load(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let args: NameArgs = serde_json::from_value(request.call.arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("capability.load args: {e}")))?;
        self.load(&args.name)?;
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
        self.unload(&args.name)?;
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
        assert!(names.contains(&"capability.search".to_string()));
        assert!(names.contains(&"capability.load".to_string()));
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
    async fn context_meta_tools_attach_typed_directives() {
        let dispatcher = dispatcher().await;
        let names = surface(&dispatcher);
        for name in [
            "context.gc_hint",
            "context.tag",
            "context.lease",
            "context.collect",
        ] {
            assert!(
                names.contains(&name.to_string()),
                "{name} must be on the default surface: {names:?}"
            );
        }

        let item_id = "00000000-0000-0000-0000-000000000000";

        // The context meta-tools return a `RuntimeDirective` (a distinct
        // `ToolOutcome` variant), not a field on the output.
        let directive = |outcome: ToolOutcome| match outcome {
            ToolOutcome::RuntimeDirective { directive, .. } => directive,
            other => panic!("context tools must return a directive, got {other:?}"),
        };

        let output = dispatcher
            .execute(request(
                "context.gc_hint",
                json!({"item_id": item_id, "keep": true}),
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
                "context.tag",
                json!({"item_id": item_id, "tag": "urgent"}),
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
                "context.lease",
                json!({"item_id": item_id, "turns": 3}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::Lease { turns: 3, .. })
        ));

        let output = dispatcher
            .execute(request("context.collect", json!({})))
            .await
            .unwrap();
        assert!(matches!(
            directive(output),
            agent_contracts::RuntimeDirective::Context(ContextAction::Collect)
        ));

        // Bad arguments are rejected like any other tool.
        let error = dispatcher
            .execute(request("context.gc_hint", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("args"), "{error}");
    }

    #[tokio::test]
    async fn context_query_tools_emit_engine_queries() {
        let dispatcher = dispatcher().await;

        // The retrieval loop is read-only and engine-serviced: the tools
        // name what they want (`EngineQuery`), the kernel resolves it.
        let query = |outcome: ToolOutcome| match outcome {
            ToolOutcome::EngineQuery { query, .. } => query,
            other => panic!("context query tools must emit an engine query, got {other:?}"),
        };

        let outcome = dispatcher
            .execute(request(
                "context.search",
                json!({"query": "AuthService", "limit": 8}),
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
            .execute(request("context.inspect", json!({"item_id": item_id})))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::InspectExternal { .. }
        ));

        let outcome = dispatcher
            .execute(request("context.fetch", json!({"item_id": item_id})))
            .await
            .unwrap();
        assert!(matches!(
            query(outcome),
            agent_contracts::EngineQuery::FetchExternal { .. }
        ));

        // Bad arguments are rejected like any other tool.
        let error = dispatcher
            .execute(request("context.search", json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("args"), "{error}");
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
            .execute(request(CAPABILITY_SEARCH, json!({"query": "git"})))
            .await
            .unwrap();
        let search = value(search);
        assert!(search.ok);
        assert!(search.model_content.contains("git.status"));

        let load = dispatcher
            .execute(request(CAPABILITY_LOAD, json!({"name": "git.status"})))
            .await
            .unwrap();
        let load = value(load);
        assert!(load.ok);
        assert!(surface(&dispatcher).contains(&"git.status".to_string()));
    }
}

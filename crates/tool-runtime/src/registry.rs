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
    AgentError, AgentResult, ToolDispatcher, ToolExecutionRequest, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use serde::Deserialize;
use serde_json::json;

use crate::tools::{
    EditReplaceTool, FsListTool, FsReadTool, FsWriteTool, GitDiffTool, GitStatusTool,
    SearchGrepTool, ShellExecTool, Tool,
};

/// Control tools that are always visible, so the model can discover and
/// change the active set no matter what else is loaded.
pub const CAPABILITY_SEARCH: &str = "capability.search";
pub const CAPABILITY_LOAD: &str = "capability.load";
pub const CAPABILITY_UNLOAD: &str = "capability.unload";

/// Lifecycle state of one catalog tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLifecycle {
    /// Registered in the catalog but never loaded.
    Available,
    /// In the active set: its schema is exposed to the model.
    Loaded,
    /// Executing a call right now.
    Active,
    /// Idle; kept only for a fast reload.
    Warm,
    /// Removed from the model surface by the GC.
    Unloaded,
}

impl ToolLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Loaded => "loaded",
            Self::Active => "active",
            Self::Warm => "warm",
            Self::Unloaded => "unloaded",
        }
    }

    fn in_surface(self) -> bool {
        matches!(self, Self::Loaded | Self::Active)
    }
}

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
            always_loaded: vec!["fs.list".into(), "fs.read".into(), "search.grep".into()],
            idle_to_warm_ticks: 8,
            warm_to_unload_ticks: 24,
        }
    }
}

/// One row of `capability.search`.
#[derive(Debug, Clone)]
pub struct ToolCatalogEntry {
    pub name: String,
    pub state: ToolLifecycle,
    pub description: String,
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
        Ok(())
    }

    /// Snapshot of the catalog for `capability.search`.
    pub fn catalog(&self) -> Vec<ToolCatalogEntry> {
        let catalog = self.catalog.read().expect("tool catalog poisoned");
        let mut entries: Vec<_> = catalog
            .iter()
            .map(|(name, entry)| ToolCatalogEntry {
                name: name.clone(),
                state: entry.state,
                description: entry.tool.spec().description.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    fn tick_now(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Age transitions on every model request: idle tools cool Loaded ->
    /// Warm and then Warm -> Unloaded, so the model surface tracks recent
    /// use. Core tools never age out.
    fn gc(&self) {
        let tick = self.tick_now();
        let mut catalog = self.catalog.write().expect("tool catalog poisoned");
        for (name, entry) in catalog.iter_mut() {
            if self.config.always_loaded.iter().any(|core| core == name) {
                continue;
            }
            let idle = tick.saturating_sub(entry.last_used_tick);
            match entry.state {
                ToolLifecycle::Loaded if idle >= self.config.idle_to_warm_ticks as u64 => {
                    entry.state = ToolLifecycle::Warm;
                }
                ToolLifecycle::Warm if idle >= self.config.warm_to_unload_ticks as u64 => {
                    entry.state = ToolLifecycle::Unloaded;
                }
                _ => {}
            }
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
        self.gc();
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

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutput> {
        let name = request.call.name.clone();
        match name.as_str() {
            CAPABILITY_SEARCH => self.run_search(request).await,
            CAPABILITY_LOAD => self.run_load(request).await,
            CAPABILITY_UNLOAD => self.run_unload(request).await,
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
    use agent_contracts::{CancellationToken, ToolCall, ToolExecutionRequest};
    use serde_json::Value;

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
        // Each specs() call advances the GC tick.
        for _ in 0..3 {
            let _ = surface(&dispatcher);
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
        assert!(search.ok);
        assert!(search.model_content.contains("git.status"));

        let load = dispatcher
            .execute(request(CAPABILITY_LOAD, json!({"name": "git.status"})))
            .await
            .unwrap();
        assert!(load.ok);
        assert!(surface(&dispatcher).contains(&"git.status".to_string()));
    }
}

//! Context meta-tools: `context.gc_hint` / `context.tag` / `context.lease` /
//! `context.collect` (directives) and `context.search` / `context.inspect` /
//! `context.fetch` (read-only engine queries).
//!
//! The directive tools do no work themselves — each attaches a typed
//! `ContextAction` to its output that the runtime routes to the context
//! engine (invariant 3: tools never touch the engine, and the kernel
//! decides how the directive is applied). The query tools attach a typed
//! `EngineQuery` that the kernel resolves against the engine — same
//! invariant, same direction: the tool only names *what* it wants, the
//! runtime answers. The model targets items by the ids it sees in the
//! materialized context frame; a stale id is a silent no-op in the engine,
//! so the tools are safe to call even when the target just left.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, ContextAction, ContextItemId, ContextKind,
    ContextScope, EngineQuery, RunId, TaskId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;

/// Which of the four meta-tools this instance serves. One struct, four
/// names — the schemas and the produced directive differ, nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextDirectiveKind {
    GcHint,
    Tag,
    Lease,
    Collect,
}

pub(crate) struct ContextDirectiveTool {
    kind: ContextDirectiveKind,
}

impl ContextDirectiveTool {
    pub(crate) fn gc_hint() -> Self {
        Self {
            kind: ContextDirectiveKind::GcHint,
        }
    }

    pub(crate) fn tag() -> Self {
        Self {
            kind: ContextDirectiveKind::Tag,
        }
    }

    pub(crate) fn lease() -> Self {
        Self {
            kind: ContextDirectiveKind::Lease,
        }
    }

    pub(crate) fn collect() -> Self {
        Self {
            kind: ContextDirectiveKind::Collect,
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            ContextDirectiveKind::GcHint => "context.gc_hint",
            ContextDirectiveKind::Tag => "context.tag",
            ContextDirectiveKind::Lease => "context.lease",
            ContextDirectiveKind::Collect => "context.collect",
        }
    }
}

#[derive(Deserialize)]
struct IdKeepArgs {
    item_id: ContextItemId,
    keep: bool,
}

#[derive(Deserialize)]
struct IdTagArgs {
    item_id: ContextItemId,
    tag: String,
}

#[derive(Deserialize)]
struct IdTurnsArgs {
    item_id: ContextItemId,
    turns: u32,
}

#[async_trait]
impl Tool for ContextDirectiveTool {
    fn spec(&self) -> ToolSpec {
        let (description, input_schema) = match self.kind {
            ContextDirectiveKind::GcHint => (
                "Ask the context engine to keep an item resident across GC passes until a later gc_hint with keep=false clears it.",
                json!({
                    "type": "object",
                    "required": ["item_id", "keep"],
                    "properties": {
                        "item_id": {"type": "string", "description": "Item id from the materialized context frame"},
                        "keep": {"type": "boolean", "description": "true protects the item, false releases it"}
                    }
                }),
            ),
            ContextDirectiveKind::Tag => (
                "Attach an extension tag to an item so later inspection can find it.",
                json!({
                    "type": "object",
                    "required": ["item_id", "tag"],
                    "properties": {
                        "item_id": {"type": "string", "description": "Item id from the materialized context frame"},
                        "tag": {"type": "string", "description": "Tag text (stored under the ext: namespace)"}
                    }
                }),
            ),
            ContextDirectiveKind::Lease => (
                "Protect an item from GC for the next N turns (the item stays resident while leased).",
                json!({
                    "type": "object",
                    "required": ["item_id", "turns"],
                    "properties": {
                        "item_id": {"type": "string", "description": "Item id from the materialized context frame"},
                        "turns": {"type": "integer", "minimum": 1, "description": "How many turns the item stays protected"}
                    }
                }),
            ),
            ContextDirectiveKind::Collect => (
                "Run a full GC pass now (manual collect): evicts what the working set no longer needs, reversibly.",
                json!({"type": "object"}),
            ),
        };
        ToolSpec {
            name: self.name().into(),
            description: description.into(),
            input_schema,
            risk: ToolRisk::ReadOnly,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let action = match self.kind {
            ContextDirectiveKind::GcHint => {
                let args: IdKeepArgs = serde_json::from_value(arguments).map_err(|e| {
                    agent_contracts::AgentError::InvalidRequest(format!(
                        "{} args: {e}",
                        self.name()
                    ))
                })?;
                ContextAction::GcHint {
                    item_id: args.item_id,
                    keep_alive: args.keep,
                }
            }
            ContextDirectiveKind::Tag => {
                let args: IdTagArgs = serde_json::from_value(arguments).map_err(|e| {
                    agent_contracts::AgentError::InvalidRequest(format!(
                        "{} args: {e}",
                        self.name()
                    ))
                })?;
                ContextAction::Tag {
                    item_id: args.item_id,
                    tag: args.tag,
                }
            }
            ContextDirectiveKind::Lease => {
                let args: IdTurnsArgs = serde_json::from_value(arguments).map_err(|e| {
                    agent_contracts::AgentError::InvalidRequest(format!(
                        "{} args: {e}",
                        self.name()
                    ))
                })?;
                ContextAction::Lease {
                    item_id: args.item_id,
                    turns: args.turns,
                }
            }
            ContextDirectiveKind::Collect => ContextAction::Collect,
        };
        let description = describe(&action);
        // The meta-tools produce a `RuntimeDirective`, not a plain output:
        // context control is a distinct `ToolOutcome` variant so only
        // trusted tools (and capabilities granted `runtime:context-control`)
        // can ask the runtime to change context state.
        Ok(ToolOutcome::RuntimeDirective {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: self.name().into(),
                ok: true,
                summary: description.clone(),
                model_content: description,
                artifact_ref: None,
                metadata: json!({"context_action": action}),
            },
            directive: agent_contracts::RuntimeDirective::Context(action),
        })
    }
}

/// Human-readable form of the directive, for the model and the summary.
fn describe(action: &ContextAction) -> String {
    match action {
        ContextAction::GcHint {
            item_id,
            keep_alive,
        } => format!("gc_hint: keep item {item_id} alive = {keep_alive}"),
        ContextAction::Tag { item_id, tag } => format!("tag: '{tag}' on item {item_id}"),
        ContextAction::Lease { item_id, turns } => {
            format!("lease: item {item_id} protected for {turns} turns")
        }
        ContextAction::Collect => "collect: full GC pass requested".to_string(),
    }
}

/// Which read-only engine query this instance serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextQueryKind {
    Search,
    Inspect,
    Fetch,
}

/// `context.search` / `context.inspect` / `context.fetch`: the on-demand
/// retrieval loop for externalized refs. These tools produce an
/// `EngineQuery` the kernel resolves against the context engine — they do
/// not touch the engine themselves and carry no side effects, so they are
/// plain `ReadOnly` tools (no `runtime:context-control` permission needed;
/// unlike directives they cannot change runtime state).
pub(crate) struct ContextQueryTool {
    kind: ContextQueryKind,
}

impl ContextQueryTool {
    pub(crate) fn search() -> Self {
        Self {
            kind: ContextQueryKind::Search,
        }
    }

    pub(crate) fn inspect() -> Self {
        Self {
            kind: ContextQueryKind::Inspect,
        }
    }

    pub(crate) fn fetch() -> Self {
        Self {
            kind: ContextQueryKind::Fetch,
        }
    }

    fn name(&self) -> &'static str {
        match self.kind {
            ContextQueryKind::Search => "context.search",
            ContextQueryKind::Inspect => "context.inspect",
            ContextQueryKind::Fetch => "context.fetch",
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    kind: Option<ContextKind>,
    #[serde(default)]
    scope: Option<ContextScope>,
    #[serde(default)]
    task_id: Option<TaskId>,
}

#[derive(Deserialize)]
struct IdArgs {
    item_id: ContextItemId,
}

#[async_trait]
impl Tool for ContextQueryTool {
    fn spec(&self) -> ToolSpec {
        let (description, input_schema) = match self.kind {
            ContextQueryKind::Search => (
                "Deterministic search over externalized context refs (items whose content was archived to the context store). Matches entity signatures, kind, scope and task, ranked by entity match then recency. Returns refs only — fetch one to see the full content.",
                json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {"type": "string", "description": "Free text; matched against entity signatures and summaries"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 64, "description": "Max refs to return (default 16)"},
                        "kind": {"type": "string", "description": "Optional ContextKind filter"},
                        "scope": {"type": "string", "description": "Optional ContextScope filter"},
                        "task_id": {"type": "string", "description": "Optional TaskId filter"}
                    }
                }),
            ),
            ContextQueryKind::Inspect => (
                "Metadata of one externalized ref by item id (kind, scope, task, residency, semantic state, tags, entities). No store read.",
                json!({
                    "type": "object",
                    "required": ["item_id"],
                    "properties": {
                        "item_id": {"type": "string", "description": "Item id from the materialized external refs or context.search"}
                    }
                }),
            ),
            ContextQueryKind::Fetch => (
                "Pull the full content of one externalized item back from the context store. The item stays externalized — this is a deliberate read, not a working-set reactivation.",
                json!({
                    "type": "object",
                    "required": ["item_id"],
                    "properties": {
                        "item_id": {"type": "string", "description": "Item id from the materialized external refs or context.search"}
                    }
                }),
            ),
        };
        ToolSpec {
            name: self.name().into(),
            description: description.into(),
            input_schema,
            risk: ToolRisk::ReadOnly,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let query = match self.kind {
            ContextQueryKind::Search => {
                let args: SearchArgs = serde_json::from_value(arguments).map_err(|e| {
                    AgentError::InvalidRequest(format!("{} args: {e}", self.name()))
                })?;
                EngineQuery::SearchExternal {
                    query: args.query,
                    kind: args.kind,
                    scope: args.scope,
                    task_id: args.task_id,
                    limit: args.limit.unwrap_or(16),
                }
            }
            ContextQueryKind::Inspect => {
                let args: IdArgs = serde_json::from_value(arguments).map_err(|e| {
                    AgentError::InvalidRequest(format!("{} args: {e}", self.name()))
                })?;
                EngineQuery::InspectExternal {
                    item_id: args.item_id,
                }
            }
            ContextQueryKind::Fetch => {
                let args: IdArgs = serde_json::from_value(arguments).map_err(|e| {
                    AgentError::InvalidRequest(format!("{} args: {e}", self.name()))
                })?;
                EngineQuery::FetchExternal {
                    item_id: args.item_id,
                }
            }
        };
        Ok(ToolOutcome::EngineQuery {
            // Placeholder: the kernel replaces the content with the
            // engine's answer (call id / tool name survive).
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: self.name().into(),
                ok: true,
                summary: "querying the context engine".into(),
                model_content: "resolving...".into(),
                artifact_ref: None,
                metadata: json!({"engine_query": true}),
            },
            query,
        })
    }
}

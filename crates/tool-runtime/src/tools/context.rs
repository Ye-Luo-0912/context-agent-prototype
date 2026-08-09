//! The merged context meta-tool: one `context.manage` entry point with an
//! `op` dispatch covering the four directives (`gc_hint` / `tag` / `lease` /
//! `collect`) and the three retrieval queries (`search` / `inspect` /
//! `fetch`).
//!
//! The directive ops do no work themselves — each attaches a typed
//! `ContextAction` to its output that the runtime routes to the context
//! engine (invariant 3: tools never touch the engine, and the kernel
//! decides how the directive is applied). The query ops attach a typed
//! `EngineQuery` that the kernel resolves against the engine — same
//! invariant, same direction: the tool only names *what* it wants, the
//! runtime answers. The model targets items by the ids it sees in the
//! materialized context frame; a stale id is a silent no-op in the engine,
//! so the tool is safe to call even when the target just left.
//!
//! One schema instead of seven keeps the always-visible tool surface
//! small: a dozen single-purpose meta-tools would cost more model input
//! than the runtime control they provide.

use agent_contracts::{
    AgentError, AgentResult, CONTEXT_MANAGE, CancellationToken, ContextAction, ContextItemId,
    ContextKind, ContextScope, EngineQuery, RunId, TaskId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;

/// Which operation `context.manage` serves this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManageOp {
    /// Keep an item resident across GC passes until a later `keep=false`.
    GcHint,
    /// Attach an extension tag to an item so later inspection can find it.
    Tag,
    /// Protect an item from GC for the next N turns.
    Lease,
    /// Run a full GC pass now (manual collect).
    Collect,
    /// Deterministic search over externalized refs.
    Search,
    /// Metadata of one externalized ref by item id (no store read).
    Inspect,
    /// Pull the full content of one externalized item back from the store.
    Fetch,
    /// Re-enter an externalized ref into the working set under its original
    /// id (one lifecycle transition; identity preserved).
    Admit,
    /// Persist a fact derived from a ref as a new item with a `DerivedFrom`
    /// link to the source.
    Derive,
}

#[derive(Deserialize)]
struct ManageArgs {
    op: ManageOp,
    // Directives
    #[serde(default)]
    item_id: Option<ContextItemId>,
    #[serde(default)]
    keep: Option<bool>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    turns: Option<u32>,
    // Retrieval
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    kind: Option<ContextKind>,
    #[serde(default)]
    scope: Option<ContextScope>,
    #[serde(default)]
    task_id: Option<TaskId>,
    // Admit / derive
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    fact: Option<String>,
}

pub(crate) struct ContextManageTool;

impl ContextManageTool {
    pub(crate) fn new() -> Self {
        Self
    }
}

fn require<T>(value: Option<T>, op: &str, field: &str) -> AgentResult<T> {
    value.ok_or_else(|| {
        AgentError::InvalidRequest(format!("context.manage {op}: missing '{field}'"))
    })
}

#[async_trait]
impl Tool for ContextManageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CONTEXT_MANAGE.into(),
            description: concat!(
                "One entry point for runtime context control and the externalized-ref retrieval loop. ",
                "Directive ops (gc_hint/tag/lease/collect) ask the runtime to change context state; ",
                "admit re-enters an externalized ref into the working set under its original id ",
                "(one lifecycle transition, identity preserved); derive persists a fact as a new ",
                "item with a DerivedFrom link to the ref. Query ops (search/inspect/fetch) read ",
                "externalized refs — search lists refs matching an entity/kind/scope/task query, ",
                "inspect shows one ref's metadata, fetch pulls its full content back on demand. ",
                "Item ids come from the materialized context frame."
            )
            .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["gc_hint", "tag", "lease", "collect", "search", "inspect", "fetch", "admit", "derive"]
                    },
                    "item_id": {"type": "string", "description": "Target item (gc_hint/tag/lease/inspect/fetch/admit/derive)"},
                    "keep": {"type": "boolean", "description": "gc_hint: true protects, false releases"},
                    "tag": {"type": "string", "description": "tag: tag text (stored under the ext: namespace)"},
                    "turns": {"type": "integer", "minimum": 1, "description": "lease: how many turns the item stays protected"},
                    "reason": {"type": "string", "description": "admit: why this ref is being pulled back into the working set"},
                    "fact": {"type": "string", "description": "derive: the fact to persist as a new derived item"},
                    "query": {"type": "string", "description": "search: free text matched against entity signatures and summaries"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 64, "description": "search: max refs to return (default 16)"},
                    "kind": {"type": "string", "description": "search: optional ContextKind filter"},
                    "scope": {"type": "string", "description": "search: optional ContextScope filter"},
                    "task_id": {"type": "string", "description": "search: optional TaskId filter"}
                }
            }),
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
        let args: ManageArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("context.manage args: {e}")))?;
        match args.op {
            // ---- Directives: a typed ContextAction the runtime routes ----
            ManageOp::GcHint => {
                let item_id = require(args.item_id, "gc_hint", "item_id")?;
                let keep_alive = require(args.keep, "gc_hint", "keep")?;
                let action = ContextAction::GcHint {
                    item_id,
                    keep_alive,
                };
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: description.clone(),
                        model_content: description,
                        artifact_ref: None,
                        metadata: json!({"context_action": action}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            ManageOp::Tag => {
                let item_id = require(args.item_id, "tag", "item_id")?;
                let tag = require(args.tag, "tag", "tag")?;
                let action = ContextAction::Tag { item_id, tag };
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: description.clone(),
                        model_content: description,
                        artifact_ref: None,
                        metadata: json!({"context_action": action}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            ManageOp::Lease => {
                let item_id = require(args.item_id, "lease", "item_id")?;
                let turns = require(args.turns, "lease", "turns")?;
                let action = ContextAction::Lease { item_id, turns };
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: description.clone(),
                        model_content: description,
                        artifact_ref: None,
                        metadata: json!({"context_action": action}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            ManageOp::Collect => {
                let action = ContextAction::Collect;
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: description.clone(),
                        model_content: description,
                        artifact_ref: None,
                        metadata: json!({"context_action": action}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            // ---- Retrieval: a typed EngineQuery the kernel resolves ----
            ManageOp::Search => {
                let query = require(args.query, "search", "query")?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::SearchExternal {
                        query,
                        kind: args.kind,
                        scope: args.scope,
                        task_id: args.task_id,
                        limit: args.limit.unwrap_or(16),
                    },
                })
            }
            ManageOp::Inspect => {
                let item_id = require(args.item_id, "inspect", "item_id")?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::InspectExternal { item_id },
                })
            }
            ManageOp::Fetch => {
                let item_id = require(args.item_id, "fetch", "item_id")?;
                Ok(ToolOutcome::EngineQuery {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "querying the context engine".into(),
                        model_content: "resolving...".into(),
                        artifact_ref: None,
                        metadata: json!({"engine_query": true}),
                    },
                    query: EngineQuery::FetchExternal { item_id },
                })
            }
            // ---- Admit / derive: typed ContextActions the runtime routes ----
            ManageOp::Admit => {
                let item_id = require(args.item_id, "admit", "item_id")?;
                let reason = require(args.reason, "admit", "reason")?;
                let action = ContextAction::Admit { item_id, reason };
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: description.clone(),
                        model_content: description,
                        artifact_ref: None,
                        metadata: json!({"context_action": action}),
                    },
                    directive: agent_contracts::RuntimeDirective::Context(action),
                })
            }
            ManageOp::Derive => {
                let item_id = require(args.item_id, "derive", "item_id")?;
                let fact = require(args.fact, "derive", "fact")?;
                let reason = require(args.reason, "derive", "reason")?;
                let action = ContextAction::Derive {
                    item_id,
                    fact,
                    reason,
                };
                let description = describe(&action);
                Ok(ToolOutcome::RuntimeDirective {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: CONTEXT_MANAGE.into(),
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
        ContextAction::Admit { item_id, reason } => {
            format!("admit: item {item_id} re-enters the working set — {reason}")
        }
        ContextAction::Derive {
            item_id,
            fact,
            reason,
        } => {
            format!("derive: '{fact}' persisted as a new item derived from {item_id} — {reason}")
        }
    }
}

//! The merged context meta-tool: one `context.manage` entry point with an
//! `op` dispatch covering catalog retrieval (`search` / `inspect` / `fetch`)
//! and deliberate mutations (`tag` / `lease` / `admit` / `derive`).
//!
//! GC / collect is not a model-facing op: the engine owns collection.
//! `ContextAction::Collect` and `ContextAction::GcHint` remain for tests
//! and the engine; a model `op=collect` or `op=gc_hint` is an invalid
//! request.
//!
//! The directive ops do no work themselves — each attaches a typed
//! `ContextAction` to its output that the runtime routes to the context
//! engine (invariant 3: tools never touch the engine, and the kernel
//! decides how the directive is applied). The query ops attach a typed
//! `EngineQuery` that the kernel resolves against the engine — same
//! invariant, same direction: the tool only names *what* it wants, the
//! runtime answers. Item ids accept a bare UUID or the catalog uri
//! `context://run/<uuid>` that search hits return.

use agent_contracts::{
    AgentError, AgentResult, CONTEXT_MANAGE, CancellationToken, ContextAction, ContextItemId,
    ContextKind, ContextScope, EngineQuery, RunId, TaskId, ToolOutcome, ToolOutput, ToolRisk,
    ToolSemanticRole, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;

/// Which operation `context.manage` serves this call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManageOp {
    /// Attach an extension tag to an item so later inspection can find it.
    Tag,
    /// Protect an item from GC for the next N turns.
    Lease,
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
    #[serde(default)]
    label: Option<String>,
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

fn search_query(args: &ManageArgs) -> AgentResult<String> {
    let has_filter = args.kind.is_some()
        || args.scope.is_some()
        || args.task_id.is_some()
        || args.label.is_some();
    match args.query.as_deref().map(str::trim) {
        Some(query) if !query.is_empty() => Ok(query.to_string()),
        _ if has_filter => Ok(String::new()),
        _ => Err(AgentError::InvalidRequest(
            "context.manage search: missing 'query'".into(),
        )),
    }
}

#[async_trait]
impl Tool for ContextManageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: CONTEXT_MANAGE.into(),
            description: concat!(
                "Runtime context control and catalog retrieval. ",
                "Directive ops: tag, lease, admit, derive. ",
                "Query ops: search, inspect, fetch. search covers the whole catalog (Resident/Warm/Stored), not only the selected working context. Hits include id, source, and residency. fetch returns the catalog or stored body; catalog residency is not the selected working set. ",
                "item_id accepts a bare UUID or the catalog uri context://run/<uuid>."
            )
            .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["tag", "lease", "search", "inspect", "fetch", "admit", "derive"]
                    },
                    "item_id": {"type": "string", "description": "Target item (tag/lease/inspect/fetch/admit/derive). Bare UUID or context://run/<uuid>."},
                    "tag": {"type": "string", "description": "tag: tag text (stored under the ext: namespace)"},
                    "turns": {"type": "integer", "minimum": 1, "description": "lease: how many turns the item stays protected"},
                    "reason": {"type": "string", "description": "admit: why this ref is being pulled back into the working set"},
                    "fact": {"type": "string", "description": "derive: the fact to persist as a new derived item"},
                    "query": {"type": "string", "description": "search: free text over entity, path, label, and summary across the catalog"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 64, "description": "search: max refs to return (default 16)"},
                    "kind": {"type": "string", "description": "search: optional ContextKind filter"},
                    "scope": {"type": "string", "description": "search: optional ContextScope filter"},
                    "task_id": {"type": "string", "description": "search: optional TaskId filter"},
                    "label": {"type": "string", "description": "search: optional label filter (decision, open-loop, ext:...)"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: vec![ToolSemanticRole::Search],
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ManageArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("context.manage args: {e}")))?;
        match args.op {
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
            ManageOp::Search => {
                let query = search_query(&args)?;
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
                        label: args.label,
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
        // runtime 推送的整组根声明投影；模型不应看到完整列表（有界摘要）。
        ContextAction::AnchorRoots { roots } => {
            format!(
                "anchor roots: {} claims projected from the task anchor",
                roots.len()
            )
        }
        ContextAction::CheckedFiles { files } => {
            format!(
                "checked files: {} paths projected from task progress",
                files.len()
            )
        }
    }
}

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
use std::str::FromStr;

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
    // Keep the union's textual fields raw until `op` is known. Model
    // clients sometimes serialize unused optional properties as ""; an
    // irrelevant placeholder must not prevent a valid operation from being
    // dispatched. Fields used by the selected op are still parsed strictly.
    // Directives
    #[serde(default)]
    item_id: Option<String>,
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
    kind: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
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

fn optional_text(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn require_text(value: &Option<String>, op: &str, field: &str) -> AgentResult<String> {
    optional_text(value).map(str::to_string).ok_or_else(|| {
        AgentError::InvalidRequest(format!("context.manage {op}: missing '{field}'"))
    })
}

fn parse_item_id(value: &Option<String>, op: &str) -> AgentResult<ContextItemId> {
    let value = require_text(value, op, "item_id")?;
    ContextItemId::from_str(&value).map_err(|error| {
        AgentError::InvalidRequest(format!("context.manage {op}: invalid 'item_id': {error}"))
    })
}

fn parse_task_id(value: &Option<String>) -> AgentResult<Option<TaskId>> {
    optional_text(value)
        .map(|value| {
            TaskId::from_str(value).map_err(|error| {
                AgentError::InvalidRequest(format!(
                    "context.manage search: invalid 'task_id': {error}"
                ))
            })
        })
        .transpose()
}

fn parse_kind(value: &Option<String>) -> AgentResult<Option<ContextKind>> {
    let Some(value) = optional_text(value) else {
        return Ok(None);
    };
    let kind = match value {
        "Goal" | "goal" => ContextKind::Goal,
        "Constraint" | "constraint" => ContextKind::Constraint,
        "Decision" | "decision" => ContextKind::Decision,
        "UserMessage" | "user_message" => ContextKind::UserMessage,
        "AssistantMessage" | "assistant_message" => ContextKind::AssistantMessage,
        "ToolObservation" | "tool_observation" => ContextKind::ToolObservation,
        "FileObservation" | "file_observation" => ContextKind::FileObservation,
        "Error" | "error" => ContextKind::Error,
        "Summary" | "summary" => ContextKind::Summary,
        "Note" | "note" => ContextKind::Note,
        other => {
            return Err(AgentError::InvalidRequest(format!(
                "context.manage search: invalid 'kind' {other:?}"
            )));
        }
    };
    Ok(Some(kind))
}

fn parse_scope(value: &Option<String>) -> AgentResult<Option<ContextScope>> {
    let Some(value) = optional_text(value) else {
        return Ok(None);
    };
    let scope = match value {
        "Message" | "message" => ContextScope::Message,
        "Turn" | "turn" => ContextScope::Turn,
        "Task" | "task" => ContextScope::Task,
        "Session" | "session" => ContextScope::Session,
        "Pinned" | "pinned" => ContextScope::Pinned,
        other => {
            return Err(AgentError::InvalidRequest(format!(
                "context.manage search: invalid 'scope' {other:?}"
            )));
        }
    };
    Ok(Some(scope))
}

fn search_query(args: &ManageArgs, has_filter: bool) -> AgentResult<String> {
    match optional_text(&args.query) {
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
                    "item_id": {"type": "string", "minLength": 36, "description": "Target item (tag/lease/inspect/fetch/admit/derive). Bare UUID or context://run/<uuid>. Omit for search; never send an empty placeholder."},
                    "tag": {"type": "string", "minLength": 1, "description": "tag: tag text (stored under the ext: namespace)"},
                    "turns": {"type": "integer", "minimum": 1, "description": "lease: how many turns the item stays protected"},
                    "reason": {"type": "string", "minLength": 1, "description": "admit/derive: why this ref is being admitted or derived"},
                    "fact": {"type": "string", "minLength": 1, "description": "derive: the fact to persist as a new derived item"},
                    "query": {"type": "string", "minLength": 1, "description": "search: free text over entity, path, label, and summary across the catalog; omit when using a filter-only search"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 64, "description": "search: max refs to return (default 16)"},
                    "kind": {"type": "string", "enum": ["Goal", "Constraint", "Decision", "UserMessage", "AssistantMessage", "ToolObservation", "FileObservation", "Error", "Summary", "Note"], "description": "search only: optional ContextKind filter; omit for other operations"},
                    "scope": {"type": "string", "enum": ["Message", "Turn", "Task", "Session", "Pinned"], "description": "search only: optional ContextScope filter; omit for other operations"},
                    "task_id": {"type": "string", "minLength": 36, "description": "search only: optional TaskId UUID filter; omit for other operations"},
                    "label": {"type": "string", "minLength": 1, "description": "search only: optional label filter (decision, open-loop, ext:...); omit for other operations"}
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
                let item_id = parse_item_id(&args.item_id, "tag")?;
                let tag = require_text(&args.tag, "tag", "tag")?;
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
                let item_id = parse_item_id(&args.item_id, "lease")?;
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
                let kind = parse_kind(&args.kind)?;
                let scope = parse_scope(&args.scope)?;
                let task_id = parse_task_id(&args.task_id)?;
                let label = optional_text(&args.label).map(str::to_string);
                let query = search_query(
                    &args,
                    kind.is_some() || scope.is_some() || task_id.is_some() || label.is_some(),
                )?;
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
                        kind,
                        scope,
                        task_id,
                        label,
                        limit: args.limit.unwrap_or(16),
                    },
                })
            }
            ManageOp::Inspect => {
                let item_id = parse_item_id(&args.item_id, "inspect")?;
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
                let item_id = parse_item_id(&args.item_id, "fetch")?;
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
                let item_id = parse_item_id(&args.item_id, "admit")?;
                let reason = require_text(&args.reason, "admit", "reason")?;
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
                let item_id = parse_item_id(&args.item_id, "derive")?;
                let fact = require_text(&args.fact, "derive", "fact")?;
                let reason = require_text(&args.reason, "derive", "reason")?;
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

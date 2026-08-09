//! Context meta-tools: `context.gc_hint` / `context.tag` / `context.lease` /
//! `context.collect`.
//!
//! They do no work themselves — each attaches a typed `ContextAction` to
//! its output that the runtime routes to the context engine (invariant 3:
//! tools never touch the engine, and the kernel decides how the directive
//! is applied). The model targets items by the ids it sees in the
//! materialized context frame; a stale id is a silent no-op in the engine,
//! so the tools are safe to call even when the target just left.

use agent_contracts::{
    AgentResult, CancellationToken, ContextAction, ContextItemId, RunId, ToolOutcome, ToolOutput,
    ToolRisk, ToolSpec,
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

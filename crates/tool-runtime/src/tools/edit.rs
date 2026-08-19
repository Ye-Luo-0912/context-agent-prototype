//! `edit.replace` — exact, occurrence-aware text replacement.
//!
//! The intended primary mutating tool (instead of whole-file writes). It is
//! explicit by construction: with no `occurrence`/`replace_all`, the old text
//! must match exactly once. Every successful edit is recorded in the
//! workspace change journal (`.focus-agent/changes.jsonl`).

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolFailureClass, ToolOutcome,
    ToolOutput, ToolRisk, ToolSpec, tool_failure_output,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, candidate_regions, content_digest, hidden_path_output, ordinary_view_blocked};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

pub struct EditReplaceTool {
    workspace: Workspace,
}

impl EditReplaceTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct ReplaceArgs {
    path: String,
    old: String,
    new: String,
    /// 1-based occurrence to replace (requires `old` to appear at least that many times).
    #[serde(default)]
    occurrence: Option<usize>,
    #[serde(default)]
    replace_all: bool,
    /// The `fs.read` revision this change is based on. When present, the file
    /// must still be exactly that digest; a mismatch is `stale_revision`.
    #[serde(default)]
    base_revision: Option<String>,
}

fn display_relative(workspace: &Workspace, path: &std::path::Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[async_trait]
impl Tool for EditReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit.replace".into(),
            description: "Replace one exact substring in a workspace file. Pass `base_revision` from fs.read so a stale file is refused. Matching is exact and never fuzzy. For several hunks or revision-checked multi-edit, use edit.patch. Records the change in the workspace change journal.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "old", "new"],
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Exact text to replace (must match exactly once unless occurrence/replace_all is given)"},
                    "new": {"type": "string"},
                    "occurrence": {"type": "integer", "minimum": 1, "description": "1-based occurrence to replace"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"},
                    "base_revision": {"type": "string", "description": "The fs.read `revision` this change is based on; a mismatch refuses the edit"}
                }
            }),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: ReplaceArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("edit.replace args: {e}")))?;
        if args.old.is_empty() {
            return Err(AgentError::InvalidRequest(
                "edit.replace `old` must not be empty".into(),
            ));
        }
        if args.occurrence.is_some() && args.replace_all {
            return Err(AgentError::InvalidRequest(
                "edit.replace: `occurrence` and `replace_all` are mutually exclusive".into(),
            ));
        }
        if ordinary_view_blocked(&args.path) {
            return Ok(ToolOutcome::Value(hidden_path_output(
                call_id,
                "edit.replace",
                &args.path,
            )));
        }

        // Reject state-dir targets up front (reads may legitimately reach
        // into artifacts; editing them is a mutation policy decision).
        let path = self.workspace.resolve_mutation(&args.path).await?;
        // Validation and open are fused into a directory-handle-relative
        // descent; the size check and the content read both go through the
        // pinned handle, so a link swap cannot redirect them.
        let confined = self.workspace.confined_open_read(&args.path).await?;
        let metadata = confined
            .metadata()
            .map_err(|e| AgentError::Io(format!("metadata {}: {e}", path.display())))?;
        if metadata.len() > MAX_FILE_BYTES {
            return Err(AgentError::InvalidRequest(format!(
                "file is {} bytes; edit.replace is limited to {} bytes",
                metadata.len(),
                MAX_FILE_BYTES
            )));
        }

        use tokio::io::AsyncReadExt;
        let mut file = confined.into_tokio();
        let mut original = String::new();
        file.read_to_string(&mut original)
            .await
            .map_err(|e| AgentError::Io(format!("read {}: {e}", path.display())))?;
        let current_revision = content_digest(original.as_bytes());
        let relative = display_relative(&self.workspace, &path);
        if let Some(expected) = args.base_revision.as_deref()
            && expected != current_revision
        {
            return Ok(ToolOutcome::Value(edit_refusal(
                call_id,
                ToolFailureClass::StaleRevision,
                &relative,
                current_revision,
                &original,
                &args.old,
                "stale_revision: file changed since fs.read; re-read and retry. Matching stays exact.",
                0,
            )));
        }
        let occurrences: Vec<_> = original.match_indices(&args.old).collect();
        let count = occurrences.len();
        if count == 0 && args.replace_all {
            return Ok(ToolOutcome::Value(edit_refusal(
                call_id,
                ToolFailureClass::NoExactMatch,
                &relative,
                current_revision,
                &original,
                &args.old,
                "no_exact_match: `old` appears 0 times. Matching stays exact; re-read and supply a current anchor.",
                0,
            )));
        }

        let updated = if args.replace_all {
            original.replace(&args.old, &args.new)
        } else {
            match args.occurrence {
                Some(n) if n >= 1 && n <= count => {
                    let mut result = String::with_capacity(original.len());
                    let (start, end) = {
                        let (idx, matched) = occurrences[n - 1];
                        (idx, idx + matched.len())
                    };
                    result.push_str(&original[..start]);
                    result.push_str(&args.new);
                    result.push_str(&original[end..]);
                    result
                }
                Some(n) => {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::NoExactMatch,
                        &relative,
                        current_revision,
                        &original,
                        &args.old,
                        &format!(
                            "no_exact_match: occurrence {n} requested but `old` appears only {count} times"
                        ),
                        count,
                    )));
                }
                None if count == 1 => original.replacen(&args.old, &args.new, 1),
                None if count == 0 => {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::NoExactMatch,
                        &relative,
                        current_revision,
                        &original,
                        &args.old,
                        "no_exact_match: `old` appears 0 times. Matching stays exact; re-read and supply a current anchor.",
                        0,
                    )));
                }
                None => {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::AmbiguousMatch,
                        &relative,
                        current_revision,
                        &original,
                        &args.old,
                        &format!(
                            "ambiguous_match: `old` appears {count} times; pass `occurrence` or `replace_all`, or use edit.patch for multi-hunk work"
                        ),
                        count,
                    )));
                }
            }
        };

        if updated == original {
            return Ok(ToolOutcome::Value(ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.replace".into(),
                ok: true,
                summary: "no-op: replacement text equals original".into(),
                model_content: format!("no change: {}", display_relative(&self.workspace, &path)),
                artifact_ref: None,
                metadata: json!({
                    "path": relative,
                    "changed": false,
                    "occurrences": count,
                    "revision": current_revision,
                }),
            }));
        }

        let transaction = self
            .workspace
            .begin_mutation("edit.replace", "replace", &args.path)
            .await?;
        // The new content is staged and journaled as prepared; the atomic
        // rename (the side effect) is committed by the runtime after the
        // generation fence, so a stale operation rolls back instead of
        // silently modifying the file.
        let prepared = match effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(updated.as_bytes(), context)
                    .await?
            }
            None => transaction.prepare(updated.as_bytes()).await?,
        };
        let effect: Box<dyn Effect> = Box::new(prepared);

        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.replace".into(),
                ok: true,
                summary: format!(
                    "replaced {} occurrence(s) in {}",
                    count.min(1),
                    display_relative(&self.workspace, &path)
                ),
                model_content: format!(
                    "edit applied: {} ({} occurrence(s) of old text; bytes {} -> {})",
                    display_relative(&self.workspace, &path),
                    count,
                    metadata.len(),
                    updated.len()
                ),
                artifact_ref: None,
                metadata: json!({
                    "path": relative,
                    "changed": true,
                    "occurrences": count,
                    "bytes_before": metadata.len(),
                    "bytes_after": updated.len(),
                    "revision": content_digest(updated.as_bytes()),
                }),
            },
            effect,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn edit_refusal(
    call_id: &str,
    class: ToolFailureClass,
    path: &str,
    revision: String,
    original: &str,
    needle: &str,
    message: &str,
    match_count: usize,
) -> ToolOutput {
    let candidates = candidate_regions(original, needle);
    let candidate_text = if candidates.is_empty() {
        "no candidate".to_string()
    } else {
        candidates.join("\n---\n")
    };
    tool_failure_output(
        call_id,
        "edit.replace",
        class,
        format!("edit.replace refused: {}", class.as_str()),
        format!("{message}\npath={path}\nrevision={revision}\n{candidate_text}"),
        json!({
            "path": path,
            "revision": revision,
            "match_count": match_count,
            "candidates": candidates,
            "recovery_hint": format!("current revision {revision}; matching stays exact"),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};
    use serde_json::json;
    use tokio::fs as tfs;

    fn request(run_id: RunId, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: "edit.replace".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn replace_single_occurrence_and_journal_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("lib.rs");
        tfs::write(&file, "fn auth() {}\nfn main() { auth(); }\n")
            .await
            .unwrap();

        let tool = EditReplaceTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(
            run_id,
            json!({"path": "lib.rs", "old": "auth() {}", "new": "auth() -> bool { true }"}),
        );
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("edit.replace must prepare a committed effect");
        };
        assert!(output.ok);
        assert!(output.heats_working_set());
        assert_eq!(output.metadata["path"], "lib.rs");
        assert_eq!(
            output.metadata["revision"].as_str().unwrap().len(),
            64,
            "replace stamps a content revision"
        );
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );

        let content = tfs::read_to_string(&file).await.unwrap();
        assert!(content.contains("auth() -> bool { true }"));
        assert!(
            !content.contains("auth() {}"),
            "old snippet fully replaced: {content}"
        );

        // The change journal recorded the mutation with old content captured.
        let journal = tfs::read_to_string(dir.path().join(".focus-agent/changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["tool"], "edit.replace");
        assert_eq!(record["path"], "lib.rs");
        assert!(
            record["old_content"]
                .as_str()
                .unwrap()
                .contains("fn auth() {}")
        );
    }

    #[tokio::test]
    async fn ambiguous_match_requires_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "a b a\n").await.unwrap();

        let tool = EditReplaceTool::new(workspace.clone());
        let run_id = RunId::new();
        let result = tool
            .execute(
                run_id,
                "c",
                request(run_id, json!({"path": "f.txt", "old": "a", "new": "x"}))
                    .call
                    .arguments,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = result else {
            panic!("ambiguous match must return a typed refusal");
        };
        assert!(!output.ok);
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::AmbiguousMatch)
        );
        assert!(output.metadata.get("revision").is_some());

        // With replace_all it succeeds (staged, then committed like the
        // runtime would).
        let request = request(
            run_id,
            json!({"path": "f.txt", "old": "a", "new": "x", "replace_all": true}),
        );
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("edit.replace must prepare a committed effect");
        };
        assert!(output.ok);
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the staged effect must commit durably"
        );
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "x b x\n");
    }

    #[tokio::test]
    async fn stale_base_revision_is_typed_and_does_not_mutate() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "hello\n").await.unwrap();
        let stale = content_digest(b"hello\n");
        tfs::write(&file, "changed\n").await.unwrap();

        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "f.txt",
                    "old": "hello",
                    "new": "hi",
                    "base_revision": stale
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("stale revision must refuse without staging");
        };
        assert!(!output.ok);
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::StaleRevision)
        );
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "changed\n");
        assert!(output.model_content.contains("revision="));
    }

    #[tokio::test]
    async fn zero_matches_returns_no_exact_match_and_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tfs::write(dir.path().join("f.txt"), "alpha beta gamma\n")
            .await
            .unwrap();
        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "f.txt", "old": "delta", "new": "x"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("zero matches must be a typed refusal");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::NoExactMatch));
        assert!(
            output.model_content.contains("no candidate")
                || output.metadata["candidates"].as_array().is_some()
        );
    }
}

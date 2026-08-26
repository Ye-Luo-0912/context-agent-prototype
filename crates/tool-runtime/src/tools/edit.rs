//! `edit.replace` — exact, occurrence-aware text replacement.
//!
//! The intended primary mutating tool (instead of whole-file writes). It is
//! explicit by construction: with no `occurrence`/`replace_all`, the old text
//! must match exactly once. Every successful edit is recorded in the
//! workspace change journal (`.focus-agent/changes.jsonl`).

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolExecutionFacts,
    ToolFailureClass, ToolOutcome, ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec,
    tool_failure_output,
};
use agent_workspace::{MAX_MUTATION_BYTES, Workspace};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ExactMatchError, LineEnding, Tool, adapt_edit_replacement, candidate_regions, contains_lone_cr,
    content_digest, exact_edit_match, hidden_path_output, is_not_found_error, missing_path_output,
    model_json_string, normalize_edit_line_endings, ordinary_view_blocked,
    projected_replacement_len, replace_all_logical_eol,
};

const MAX_FILE_BYTES: usize = MAX_MUTATION_BYTES;

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

#[async_trait]
impl Tool for EditReplaceTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit.replace".into(),
            description: "Replace one exact substring in a workspace file. Pass `base_revision` from fs.read so a stale file is refused. Matching is exact and never fuzzy; LF and CRLF are equivalent only as logical newline tokens, while every other byte stays exact, and written newlines preserve the matched target region. For several hunks or revision-checked multi-edit, use edit.patch. Records the change in the workspace change journal. On success the changed region and the new revision are echoed, so a chained edit needs no confirm re-read.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["path", "old", "new"],
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string", "description": "Exact text to replace (must match exactly once unless occurrence/replace_all is given; only LF/CRLF encoding is token-equivalent)"},
                    "new": {"type": "string"},
                    "occurrence": {"type": "integer", "minimum": 1, "description": "1-based occurrence to replace"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence"},
                    "base_revision": {"type": "string", "description": "The fs.read `revision` this change is based on; a mismatch refuses the edit"}
                }
            }),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
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

        // Acquire the path lease first, then read one exact bounded snapshot
        // that the returned transaction owns through commit/rollback. This
        // removes the former preflight + begin_mutation double read and
        // prevents another in-process edit from making that snapshot stale.
        let requested = vec![args.path.clone()];
        let mut snapshots = match self
            .workspace
            .begin_existing_mutations("edit.replace", "replace", &requested, MAX_FILE_BYTES)
            .await
        {
            Ok(snapshots) => snapshots,
            Err(error) if is_not_found_error(&error) => {
                return Ok(ToolOutcome::Value(
                    missing_path_output(&self.workspace, call_id, "edit.replace", &args.path).await,
                ));
            }
            Err(error) => return Err(error),
        };
        let snapshot = snapshots
            .pop()
            .expect("one requested edit target yields one snapshot");
        let relative = snapshot.relative_path().to_string();
        let current_revision = snapshot.revision().to_string();
        let (transaction, original) = snapshot.into_parts();
        let original = String::from_utf8(original).map_err(|_| {
            AgentError::InvalidRequest(format!("edit.replace target is not UTF-8 text: {relative}"))
        })?;
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
        let line_ending = LineEnding::detect(&original);
        let old = normalize_edit_line_endings(&args.old, line_ending);
        let new = normalize_edit_line_endings(&args.new, line_ending);
        let mut line_endings_normalized = old.as_ref() != args.old || new.as_ref() != args.new;

        let (count, replacements, updated) = if args.replace_all {
            if line_ending == LineEnding::Mixed
                || (line_ending == LineEnding::CrLf && contains_lone_cr(old.as_ref()))
            {
                let result =
                    replace_all_logical_eol(&original, old.as_ref(), new.as_ref(), MAX_FILE_BYTES)
                        .map_err(|()| {
                            AgentError::InvalidRequest(format!(
                                "edit.replace result exceeds the {MAX_FILE_BYTES}-byte file limit"
                            ))
                        })?;
                if result.count == 0 {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::NoExactMatch,
                        &relative,
                        current_revision,
                        &original,
                        old.as_ref(),
                        "no_exact_match: `old` appears 0 times after logical LF/CRLF normalization. Matching stays exact; re-read and supply a current anchor.",
                        0,
                    )));
                }
                line_endings_normalized |= result.line_endings_adapted;
                (
                    result.count,
                    result.count,
                    result
                        .updated
                        .expect("a non-empty match set is transformed"),
                )
            } else {
                let count = original.match_indices(old.as_ref()).count();
                if count == 0 {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::NoExactMatch,
                        &relative,
                        current_revision,
                        &original,
                        old.as_ref(),
                        "no_exact_match: `old` appears 0 times after target line-ending normalization. Matching stays exact; re-read and supply a current anchor.",
                        0,
                    )));
                }
                projected_replacement_len(original.len(), old.len(), new.len(), count)
                    .filter(|size| *size <= MAX_FILE_BYTES)
                    .ok_or_else(|| {
                        AgentError::InvalidRequest(format!(
                            "edit.replace result exceeds the {MAX_FILE_BYTES}-byte file limit"
                        ))
                    })?;
                (count, count, original.replace(old.as_ref(), new.as_ref()))
            }
        } else {
            match exact_edit_match(&original, old.as_ref(), line_ending, args.occurrence) {
                Ok(found) => {
                    let replacement =
                        adapt_edit_replacement(&original, found, new.as_ref(), line_ending);
                    line_endings_normalized |= &original[found.start..found.end] != old.as_ref()
                        || replacement.as_ref() != new.as_ref();
                    let projected = projected_replacement_len(
                        original.len(),
                        found.end - found.start,
                        replacement.len(),
                        1,
                    )
                    .filter(|size| *size <= MAX_FILE_BYTES)
                    .ok_or_else(|| {
                        AgentError::InvalidRequest(format!(
                            "edit.replace result exceeds the {MAX_FILE_BYTES}-byte file limit"
                        ))
                    })?;
                    let mut result = String::with_capacity(projected);
                    result.push_str(&original[..found.start]);
                    result.push_str(replacement.as_ref());
                    result.push_str(&original[found.end..]);
                    (found.count, 1, result)
                }
                Err(ExactMatchError::NoMatch { count }) => {
                    let message = match args.occurrence {
                        Some(n) => format!(
                            "no_exact_match: occurrence {n} requested but `old` appears only {count} times after target line-ending normalization"
                        ),
                        None => "no_exact_match: `old` appears 0 times after target line-ending normalization. Matching stays exact; re-read and supply a current anchor.".into(),
                    };
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::NoExactMatch,
                        &relative,
                        current_revision,
                        &original,
                        old.as_ref(),
                        &message,
                        count,
                    )));
                }
                Err(ExactMatchError::Ambiguous { count }) => {
                    return Ok(ToolOutcome::Value(edit_refusal(
                        call_id,
                        ToolFailureClass::AmbiguousMatch,
                        &relative,
                        current_revision,
                        &original,
                        old.as_ref(),
                        &format!(
                            "ambiguous_match: `old` appears {count} times; pass `occurrence` or `replace_all`, or use edit.patch for multi-hunk work"
                        ),
                        count,
                    )));
                }
            }
        };

        if updated == original {
            let mut output = ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.replace".into(),
                ok: true,
                summary: "no-op: replacement text equals original".into(),
                model_content: format!("no change: {relative}"),
                artifact_ref: None,
                metadata: json!({
                    "path": relative,
                    "changed": false,
                    "occurrences": count,
                    "revision": current_revision,
                    "line_ending": line_ending.as_str(),
                    "line_endings_normalized": line_endings_normalized,
                }),
            };
            output.set_native_execution_facts(
                ToolExecutionFacts::from_resource_touches([(
                    relative.as_str(),
                    Some(current_revision.clone()),
                )])
                .with_verification(false)
                .with_mutation_bound(true),
            );
            return Ok(ToolOutcome::Value(output));
        }

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

        let new_revision = content_digest(updated.as_bytes());
        let updated_line_ending = LineEnding::detect(&updated);
        let mut output = ToolOutput {
            call_id: call_id.into(),
            tool_name: "edit.replace".into(),
            ok: true,
            summary: format!("replaced {} occurrence(s) in {}", replacements, relative),
            // The success line carries the new revision (so a chained
            // edit can pass `base_revision` without a re-read) plus a
            // bounded echo of the changed region: the model can anchor
            // its next edit on what the file actually looks like now,
            // instead of spending a confirm `fs.read` round.
            model_content: format!(
                "edit applied: {} ({} occurrence(s) of old text; bytes {} -> {}; revision {})\n{}",
                relative,
                count,
                original.len(),
                updated.len(),
                new_revision,
                super::edit_echo(&original, &updated, super::EDIT_ECHO_MAX_CHARS).trim_end()
            ),
            artifact_ref: None,
            metadata: json!({
                "path": relative,
                "changed": true,
                "occurrences": count,
                "bytes_before": original.len(),
                "bytes_after": updated.len(),
                "revision": new_revision,
                // `line_ending` describes the returned revision. Keep
                // the input style separately so chained edits never
                // receive stale metadata after adding/removing EOLs.
                "line_ending_before": line_ending.as_str(),
                "line_ending": updated_line_ending.as_str(),
                "line_endings_normalized": line_endings_normalized,
            }),
        };
        output.set_native_execution_facts(
            ToolExecutionFacts::from_resource_touches([(
                relative.as_str(),
                Some(new_revision.clone()),
            )])
            .with_verification(false)
            .with_mutation_bound(true),
        );
        Ok(ToolOutcome::PreparedEffect { output, effect })
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
    let quoted_path = model_json_string(path);
    let candidate_text = if candidates.is_empty() {
        "no candidate".to_string()
    } else {
        candidates.join("\n---\n")
    };
    let mut output = tool_failure_output(
        call_id,
        "edit.replace",
        class,
        format!("edit.replace refused: {}", class.as_str()),
        format!("{message}\npath={quoted_path}\nrevision={revision}\n{candidate_text}"),
        json!({
            "path": path,
            "revision": revision,
            "match_count": match_count,
            "candidates": candidates,
            "recovery_hint": format!("current revision {revision}; matching stays exact"),
        }),
    );
    output.set_native_execution_facts(
        ToolExecutionFacts::from_resource_touches([(path, Some(revision.clone()))])
            .with_verification(false)
            .with_mutation_bound(true),
    );
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};

    /// Applied, no-op and refused replace outcomes must stamp native facts
    /// identical to the legacy key derivation.
    #[tokio::test]
    async fn native_facts_match_the_legacy_derivation_on_every_replace_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("app.txt"), "alpha beta\n").unwrap();
        let tool = EditReplaceTool::new(workspace.clone());
        let run_id = RunId::new();

        let outcome = tool
            .execute(
                run_id,
                "e",
                json!({"path": "app.txt", "old": "alpha", "new": "gamma"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("edit.replace must prepare a committed effect");
        };
        assert!(output.ok);
        crate::tools::assert_native_facts_match_derivation(&output);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));

        let outcome = tool
            .execute(
                run_id,
                "n",
                json!({"path": "app.txt", "old": "gamma", "new": "gamma"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("no-op replacement must return a value outcome");
        };
        crate::tools::assert_native_facts_match_derivation(&output);

        let outcome = tool
            .execute(
                run_id,
                "x",
                json!({"path": "app.txt", "old": "absent", "new": "z"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("refused replacement must return a value outcome");
        };
        assert!(!output.ok);
        crate::tools::assert_native_facts_match_derivation(&output);
    }
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
        // The success line is self-sufficient for a chained edit: it
        // carries the new revision and a bounded echo of the changed
        // region, so the model needs no confirm `fs.read`.
        assert!(
            output.model_content.contains(&format!(
                "revision {}",
                output.metadata["revision"].as_str().unwrap()
            )),
            "the success line must carry the new revision: {}",
            output.model_content
        );
        assert!(
            output
                .model_content
                .contains("     1 | fn auth() -> bool { true }"),
            "the echo must show the changed region with line numbers: {}",
            output.model_content
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

    #[tokio::test]
    async fn lf_multiline_argument_edits_crlf_file_without_changing_its_style() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("windows.txt");
        tfs::write(&file, b"alpha\r\nbeta\r\ngamma\r\n")
            .await
            .unwrap();
        let revision = content_digest(b"alpha\r\nbeta\r\ngamma\r\n");

        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "windows.txt",
                    "base_revision": revision,
                    "old": "alpha\nbeta",
                    "new": "ALPHA\nBETA"
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("LF arguments must match a uniform CRLF target");
        };
        assert_eq!(output.metadata["line_ending_before"], "crlf");
        assert_eq!(output.metadata["line_ending"], "crlf");
        assert_eq!(output.metadata["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read(&file).await.unwrap(),
            b"ALPHA\r\nBETA\r\ngamma\r\n"
        );
    }

    #[tokio::test]
    async fn success_metadata_describes_the_updated_revision_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("line.txt");
        tfs::write(&file, b"seed").await.unwrap();
        let tool = EditReplaceTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "line.txt", "old": "seed", "new": "one\ntwo"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("replacement must prepare an effect");
        };
        assert_eq!(output.metadata["line_ending_before"], "none");
        assert_eq!(output.metadata["line_ending"], "lf");
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(tfs::read(&file).await.unwrap(), b"one\ntwo");
    }

    #[tokio::test]
    async fn lone_cr_cannot_split_uniform_crlf_in_single_or_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("windows.txt");
        let original = b"a\r\nb\r\n";
        tfs::write(&file, original).await.unwrap();
        let tool = EditReplaceTool::new(workspace);

        for replace_all in [false, true] {
            let outcome = tool
                .execute(
                    RunId::new(),
                    "c",
                    json!({
                        "path": "windows.txt",
                        "old": "\r",
                        "new": "X",
                        "replace_all": replace_all
                    }),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let ToolOutcome::Value(output) = outcome else {
                panic!("a lone CR anchor must refuse instead of splitting CRLF");
            };
            assert_eq!(output.failure_class(), Some(ToolFailureClass::NoExactMatch));
            assert_eq!(tfs::read(&file).await.unwrap(), original);
        }
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn logical_newline_anchor_edits_mixed_file_and_preserves_local_styles() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("mixed.txt");
        let original = b"alpha\r\nbeta\ngamma\r\n";
        tfs::write(&file, original).await.unwrap();

        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "mixed.txt",
                    "base_revision": content_digest(original),
                    "old": "alpha\nbeta\ngamma",
                    "new": "ALPHA\nBETA\nGAMMA\nEXTRA"
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("logical LF arguments must match CRLF tokens inside a mixed file");
        };
        assert_eq!(output.metadata["line_ending"], "mixed");
        assert_eq!(output.metadata["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read(&file).await.unwrap(),
            b"ALPHA\r\nBETA\nGAMMA\nEXTRA\r\n"
        );
    }

    #[tokio::test]
    async fn mixed_eol_replace_all_reconstructs_each_occurrence_locally() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("mixed.txt");
        tfs::write(&file, b"x\r\ny\n--\nx\ny\r\n").await.unwrap();

        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "mixed.txt",
                    "old": "x\ny",
                    "new": "u\nv\nw",
                    "replace_all": true
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("replace_all must transform every logical exact occurrence");
        };
        assert_eq!(output.metadata["occurrences"], 2);
        assert_eq!(output.metadata["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read(&file).await.unwrap(),
            b"u\r\nv\r\nw\n--\nu\nv\nw\r\n"
        );
    }

    #[tokio::test]
    async fn expanded_result_over_limit_is_rejected_before_staging() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "x").await.unwrap();
        let tool = EditReplaceTool::new(workspace);

        let result = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "f.txt", "old": "x", "new": "y".repeat(MAX_FILE_BYTES + 1)}),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "oversized result must be refused");
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "x");
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn missing_edit_target_returns_typed_topology_evidence() {
        let dir = tempfile::tempdir().unwrap();
        tfs::write(dir.path().join("nearby.txt"), "x")
            .await
            .unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = EditReplaceTool::new(workspace);
        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"path": "missing.txt", "old": "x", "new": "y"}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("missing target must refuse before staging");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::PathNotFound));
        assert!(output.model_content.contains("nearby.txt"));
    }
}

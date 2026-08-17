//! `edit.patch` — multi-hunk, revision-checked text patching.
//!
//! The file-revision companion to `edit.replace` (TOOLS-05): one call
//! applies several exact-match hunks to one workspace file, optionally
//! gated on a `base_revision` the model captured from `fs.read`. When the
//! revision is given, the file must still be exactly that revision, so an
//! edit based on stale content is refused instead of silently applied to
//! drifted text. Like `edit.replace`, the new content is staged as a
//! prepared effect and journaled; the runtime commits it behind the
//! generation fence.

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
const MAX_HUNKS: usize = 64;
const MAX_FILES: usize = 16;

pub struct EditPatchTool {
    workspace: Workspace,
}

impl EditPatchTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct PatchArgs {
    /// Multi-file form: each entry patches one workspace file; the whole
    /// set commits as one sequential composite effect. Each file commit is
    /// atomic, but the set is not a cross-file transaction: a later failure
    /// can require recovery of files that committed earlier.
    #[serde(default)]
    files: Vec<FilePatch>,
    /// Single-file shortcut (the original shape): `path` + `hunks` on the
    /// call itself. Mutually exclusive with `files`.
    #[serde(default)]
    path: Option<String>,
    /// Single-file shortcut: the file's `base_revision`.
    #[serde(default)]
    base_revision: Option<String>,
    #[serde(default)]
    hunks: Vec<Hunk>,
}

#[derive(Deserialize, Clone)]
struct FilePatch {
    path: String,
    /// The revision `fs.read` reported when the model read the file. When
    /// present, the file must still be exactly this revision — a mismatch
    /// refuses the edit instead of applying it to drifted content.
    #[serde(default)]
    base_revision: Option<String>,
    /// Exact-match hunks applied in order.
    hunks: Vec<Hunk>,
}

#[derive(Deserialize, Clone)]
struct Hunk {
    old: String,
    new: String,
    /// 1-based occurrence to replace when `old` appears more than once.
    #[serde(default)]
    occurrence: Option<usize>,
}

/// One resolved file patch: the confined path plus the content the hunks
/// will transform and the arguments that produced it.
struct ResolvedPatch {
    relative: String,
    original: String,
    updated: String,
    bytes_before: u64,
    hunks: usize,
}

fn display_relative(workspace: &Workspace, path: &std::path::Path) -> String {
    path.strip_prefix(workspace.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

impl EditPatchTool {
    /// Apply one exact-match hunk to the working text. The hunk's `old`
    /// must match exactly once (or a valid `occurrence` is given),
    /// mirroring `edit.replace`'s explicitness.
    fn apply_hunk(original: &str, hunk: &Hunk) -> Result<String, ToolFailureClass> {
        if hunk.old.is_empty() {
            return Err(ToolFailureClass::InvalidRequest);
        }
        let occurrences: Vec<_> = original.match_indices(&hunk.old).collect();
        let count = occurrences.len();
        let (start, end) = match hunk.occurrence {
            Some(n) if n >= 1 && n <= count => {
                let (idx, matched) = occurrences[n - 1];
                (idx, idx + matched.len())
            }
            Some(_) => {
                return Err(ToolFailureClass::NoExactMatch);
            }
            None if count == 1 => {
                let (idx, matched) = occurrences[0];
                (idx, idx + matched.len())
            }
            None if count == 0 => {
                return Err(ToolFailureClass::NoExactMatch);
            }
            None => {
                return Err(ToolFailureClass::AmbiguousMatch);
            }
        };
        let mut result = String::with_capacity(original.len() + hunk.new.len());
        result.push_str(&original[..start]);
        result.push_str(&hunk.new);
        result.push_str(&original[end..]);
        Ok(result)
    }
}

#[async_trait]
impl Tool for EditPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit.patch".into(),
            description: "Apply exact-match text hunks to one or more workspace files, optionally gated on each file's fs.read `base_revision`; edits are staged and journaled, each file commit is atomic, and a partial multi-file commit is reported for recovery.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                        "description": "Multi-file form; either this or the single-file `path`+`hunks` shortcut",
                        "items": {
                            "type": "object",
                            "required": ["path", "hunks"],
                            "properties": {
                                "path": {"type": "string"},
                                "base_revision": {"type": "string", "description": "The fs.read `revision` this change is based on; a mismatch refuses the edit (file changed since it was read)"},
                                "hunks": {
                                    "type": "array",
                                    "minItems": 1,
                                    "maxItems": 64,
                                    "items": {
                                        "type": "object",
                                        "required": ["old", "new"],
                                        "properties": {
                                            "old": {"type": "string", "description": "Exact text to replace (must match exactly once unless occurrence is given)"},
                                            "new": {"type": "string"},
                                            "occurrence": {"type": "integer", "minimum": 1, "description": "1-based occurrence to replace"}
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "path": {"type": "string", "description": "Single-file shortcut: the target path (with `hunks`)"},
                    "base_revision": {"type": "string", "description": "Single-file shortcut: the fs.read `revision` this change is based on"},
                    "hunks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "description": "Single-file shortcut: exact-match hunks applied in order",
                        "items": {
                            "type": "object",
                            "required": ["old", "new"],
                            "properties": {
                                "old": {"type": "string", "description": "Exact text to replace (must match exactly once unless occurrence is given)"},
                                "new": {"type": "string"},
                                "occurrence": {"type": "integer", "minimum": 1, "description": "1-based occurrence to replace"}
                            }
                        }
                    }
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
        let args: PatchArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("edit.patch args: {e}")))?;

        // Single-file shortcut or the multi-file form (mutually exclusive).
        let files: Vec<FilePatch> = match (&args.files, &args.path) {
            (files, None) if !files.is_empty() => files.clone(),
            (files, Some(path)) if files.is_empty() => vec![FilePatch {
                path: path.clone(),
                base_revision: args.base_revision.clone(),
                hunks: args.hunks.clone(),
            }],
            (_, Some(_)) => {
                return Err(AgentError::InvalidRequest(
                    "edit.patch: use either `files` or `path`+`hunks`, not both".into(),
                ));
            }
            (_, None) => {
                return Err(AgentError::InvalidRequest(
                    "edit.patch requires either `files` or `path`+`hunks`".into(),
                ));
            }
        };
        if files.len() > MAX_FILES {
            return Err(AgentError::InvalidRequest(format!(
                "edit.patch is limited to {MAX_FILES} files per call"
            )));
        }
        let total_hunks: usize = files.iter().map(|file| file.hunks.len()).sum();
        if total_hunks == 0 || total_hunks > MAX_HUNKS {
            return Err(AgentError::InvalidRequest(format!(
                "edit.patch requires between 1 and {MAX_HUNKS} hunks per call (got {total_hunks})"
            )));
        }

        // Phase 1 — resolve and compute every file's new content *before*
        // any mutation is staged, so an invalid hunk or stale revision
        // anywhere refuses the whole call with no side effects.
        let mut resolved: Vec<ResolvedPatch> = Vec::with_capacity(files.len());
        for file in &files {
            if ordinary_view_blocked(&file.path) {
                return Ok(ToolOutcome::Value(hidden_path_output(
                    call_id,
                    "edit.patch",
                    &file.path,
                )));
            }
            // Reject state-dir targets up front (reads may legitimately
            // reach into artifacts; editing them is a mutation policy
            // decision).
            let path = self.workspace.resolve_mutation(&file.path).await?;
            // Validation and open are fused into a directory-handle-relative
            // descent; the size check and the content read both go through
            // the pinned handle, so a link swap cannot redirect them.
            let confined = self.workspace.confined_open_read(&file.path).await?;
            let metadata = confined
                .metadata()
                .map_err(|e| AgentError::Io(format!("metadata {}: {e}", path.display())))?;
            if metadata.len() > MAX_FILE_BYTES {
                return Err(AgentError::InvalidRequest(format!(
                    "file is {} bytes; edit.patch is limited to {} bytes",
                    metadata.len(),
                    MAX_FILE_BYTES
                )));
            }

            use tokio::io::AsyncReadExt;
            let mut handle = confined.into_tokio();
            let mut original = String::new();
            handle
                .read_to_string(&mut original)
                .await
                .map_err(|e| AgentError::Io(format!("read {}: {e}", path.display())))?;

            // File-revision precondition: the model based its hunks on a
            // specific read; if the file moved on, the hunks may not apply
            // to what is actually there, so refuse instead of guessing.
            if let Some(expected) = &file.base_revision {
                let current = content_digest(original.as_bytes());
                if current != *expected {
                    let relative = display_relative(&self.workspace, &path);
                    return Ok(ToolOutcome::Value(patch_refusal(
                        call_id,
                        ToolFailureClass::StaleRevision,
                        &relative,
                        current,
                        &original,
                        file.hunks.first().map(|h| h.old.as_str()).unwrap_or(""),
                        "stale_revision: file changed since fs.read; re-read and retry. Matching stays exact.",
                        0,
                    )));
                }
            }

            let mut updated = original.clone();
            for hunk in &file.hunks {
                match Self::apply_hunk(&updated, hunk) {
                    Ok(next) => updated = next,
                    Err(ToolFailureClass::InvalidRequest) => {
                        return Err(AgentError::InvalidRequest(
                            "edit.patch hunk `old` must not be empty".into(),
                        ));
                    }
                    Err(class) => {
                        let relative = display_relative(&self.workspace, &path);
                        let current = content_digest(original.as_bytes());
                        let count = updated.match_indices(&hunk.old).count();
                        let message = match class {
                            ToolFailureClass::AmbiguousMatch => format!(
                                "ambiguous_match: hunk `old` appears {count} times; pass occurrence or use a unique exact anchor"
                            ),
                            _ => format!(
                                "no_exact_match: hunk `old` appears {count} times. Matching stays exact."
                            ),
                        };
                        return Ok(ToolOutcome::Value(patch_refusal(
                            call_id, class, &relative, current, &updated, &hunk.old, &message,
                            count,
                        )));
                    }
                }
            }
            resolved.push(ResolvedPatch {
                relative: display_relative(&self.workspace, &path),
                original,
                updated,
                bytes_before: metadata.len(),
                hunks: file.hunks.len(),
            });
        }

        // Phase 2 — stage the changed files as prepared effects. Nothing
        // lands yet: the composite effect is committed by the runtime
        // behind the generation fence. File commits are sequential, not a
        // cross-file transaction; a later failure cleans unattempted staged
        // files and reports the earlier applied files as recovery work.
        let mut effects: Vec<Box<dyn Effect>> = Vec::new();
        let mut file_reports: Vec<Value> = Vec::new();
        let mut changed_any = false;
        let mut total_hunks_applied = 0usize;
        for patch in &resolved {
            let changed = patch.updated != patch.original;
            file_reports.push(json!({
                "path": patch.relative,
                "changed": changed,
                "hunks": if changed { patch.hunks } else { 0 },
                "bytes_before": patch.bytes_before,
                "bytes_after": patch.updated.len(),
                // The revision of the applied result, so a follow-up
                // patch can chain on it.
                "revision": content_digest(patch.updated.as_bytes()),
            }));
            if changed {
                changed_any = true;
                total_hunks_applied += patch.hunks;
                let transaction = self
                    .workspace
                    .begin_mutation("edit.patch", "patch", &patch.relative)
                    .await?;
                // The new content is staged and journaled as prepared; the
                // atomic rename (the side effect) is committed by the
                // runtime after the generation fence.
                let prepared = match effect_context.clone() {
                    Some(context) => {
                        transaction
                            .prepare_with_effect_context(patch.updated.as_bytes(), context)
                            .await?
                    }
                    None => transaction.prepare(patch.updated.as_bytes()).await?,
                };
                effects.push(Box::new(prepared));
            }
        }

        if !changed_any {
            return Ok(ToolOutcome::Value(ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.patch".into(),
                ok: true,
                summary: "no-op: hunks produced no change".into(),
                model_content: "no change: all hunks matched already-applied content".into(),
                artifact_ref: None,
                metadata: json!({"changed": false, "files": file_reports}),
            }));
        }

        Ok(ToolOutcome::PreparedEffect {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "edit.patch".into(),
                ok: true,
                summary: format!(
                    "applied {} hunk(s) across {} file(s)",
                    total_hunks_applied,
                    effects.len()
                ),
                model_content: format!(
                    "patch applied: {} file(s), {} hunk(s)",
                    effects.len(),
                    total_hunks_applied
                ),
                artifact_ref: None,
                metadata: json!({"changed": true, "files": file_reports}),
            },
            // The composite effect commits changed files in order. A
            // failure cleans the unattempted preparations; already-applied
            // files remain applied and are reported as requiring recovery.
            effect: Box::new(effects),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn patch_refusal(
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
        "edit.patch",
        class,
        format!("edit.patch refused: {}", class.as_str()),
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
                name: "edit.patch".into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    async fn read_revision(workspace: &Workspace, path: &str) -> String {
        let bytes = tfs::read(workspace.root().join(path)).await.unwrap();
        content_digest(&bytes)
    }

    #[tokio::test]
    async fn patch_applies_hunks_in_order_and_journals() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("lib.rs");
        tfs::write(&file, "fn a() {}\nfn b() {}\nfn main() { a(); b(); }\n")
            .await
            .unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();
        let revision = read_revision(&workspace, "lib.rs").await;
        let request = request(
            run_id,
            json!({
                "path": "lib.rs",
                "base_revision": revision,
                "hunks": [
                    {"old": "fn a() {}", "new": "fn a() -> u8 { 0 }"},
                    {"old": "fn b() {}", "new": "fn b() -> u8 { 1 }"}
                ]
            }),
        );
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("edit.patch must prepare a committed effect");
        };
        assert!(output.ok);
        assert_eq!(output.metadata["files"][0]["hunks"], 2);
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
        assert!(content.contains("fn a() -> u8 { 0 }"));
        assert!(content.contains("fn b() -> u8 { 1 }"));
        assert!(
            !content.contains("fn a() {}") && !content.contains("fn b() {}"),
            "both hunks applied: {content}"
        );

        // The journal recorded the patch with old content captured.
        let journal = tfs::read_to_string(dir.path().join(".focus-agent/changes.jsonl"))
            .await
            .unwrap();
        let record: serde_json::Value =
            serde_json::from_str(journal.lines().next().unwrap()).unwrap();
        assert_eq!(record["kind"], "mutation_prepared");
        assert_eq!(record["tool"], "edit.patch");
        assert!(
            record["old_content"]
                .as_str()
                .unwrap()
                .contains("fn a() {}")
        );
    }

    #[tokio::test]
    async fn patch_refuses_stale_base_revision() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "original\n").await.unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();
        // The model read the file, then the file changed underneath it.
        let stale = read_revision(&workspace, "f.txt").await;
        tfs::write(&file, "changed by someone else\n")
            .await
            .unwrap();

        let result = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({
                        "path": "f.txt",
                        "base_revision": stale,
                        "hunks": [{"old": "original", "new": "patched"}]
                    }),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        let ToolOutcome::Value(output) = result.unwrap() else {
            panic!("a stale edit must refuse without staging");
        };
        assert!(!output.ok);
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::StaleRevision)
        );
        assert!(
            output.model_content.contains("stale_revision"),
            "a stale edit must be refused: {}",
            output.model_content
        );

        // The file is untouched.
        assert_eq!(
            tfs::read_to_string(&file).await.unwrap(),
            "changed by someone else\n"
        );
    }

    #[tokio::test]
    async fn patch_without_revision_still_applies() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "one two\n").await.unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(
            run_id,
            json!({
                "path": "f.txt",
                "hunks": [{"old": "one", "new": "uno"}, {"old": "two", "new": "dos"}]
            }),
        );
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("edit.patch must prepare a committed effect");
        };
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
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "uno dos\n");
    }

    #[tokio::test]
    async fn patch_rejects_ambiguous_and_missing_hunks() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "a b a\n").await.unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();

        // Ambiguous: `old` appears twice, no occurrence.
        let result = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"path": "f.txt", "hunks": [{"old": "a", "new": "x"}]}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        let ToolOutcome::Value(output) = result.unwrap() else {
            panic!("ambiguous hunks must be a typed refusal");
        };
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::AmbiguousMatch)
        );

        // Missing: the hunk's `old` is not in the file.
        let result = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({"path": "f.txt", "hunks": [{"old": "zzz", "new": "x"}]}),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        let ToolOutcome::Value(output) = result.unwrap() else {
            panic!("missing hunks must be a typed refusal");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::NoExactMatch));
    }

    #[tokio::test]
    async fn patch_applies_multiple_files_as_one_composite() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tfs::write(dir.path().join("a.txt"), "alpha\n")
            .await
            .unwrap();
        tfs::write(dir.path().join("b.txt"), "beta\n")
            .await
            .unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();
        let request = request(
            run_id,
            json!({
                "files": [
                    {"path": "a.txt", "hunks": [{"old": "alpha", "new": "ALPHA"}]},
                    {"path": "b.txt", "hunks": [{"old": "beta", "new": "BETA"}]}
                ]
            }),
        );
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("edit.patch must prepare a committed effect");
        };
        assert!(output.ok);
        assert_eq!(output.metadata["files"].as_array().unwrap().len(), 2);
        assert!(
            matches!(
                effect.commit().await,
                agent_contracts::EffectReceipt::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                    ..
                }
            ),
            "the composite effect must commit durably"
        );
        assert_eq!(
            tfs::read_to_string(dir.path().join("a.txt")).await.unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            tfs::read_to_string(dir.path().join("b.txt")).await.unwrap(),
            "BETA\n"
        );

        // Each file's mutation is journaled (rollback evidence); the
        // journal also carries the mutation's bookkeeping rows, so count
        // the patch records specifically.
        let journal = tfs::read_to_string(dir.path().join(".focus-agent/changes.jsonl"))
            .await
            .unwrap();
        let patch_records: Vec<serde_json::Value> = journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|record: &serde_json::Value| record["tool"] == "edit.patch")
            .collect();
        assert_eq!(
            patch_records.len(),
            2,
            "both mutations are journaled as patch records: {journal}"
        );
    }

    #[tokio::test]
    async fn patch_refuses_the_whole_call_when_any_file_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tfs::write(dir.path().join("a.txt"), "alpha\n")
            .await
            .unwrap();
        tfs::write(dir.path().join("b.txt"), "beta\n")
            .await
            .unwrap();

        // The model read both files, then `b.txt` changed underneath it.
        let stale = read_revision(&workspace, "b.txt").await;
        tfs::write(dir.path().join("b.txt"), "BETA by someone else\n")
            .await
            .unwrap();

        let tool = EditPatchTool::new(workspace.clone());
        let run_id = RunId::new();
        let result = tool
            .execute(
                run_id,
                "c",
                request(
                    run_id,
                    json!({
                        "files": [
                            {"path": "a.txt", "hunks": [{"old": "alpha", "new": "ALPHA"}]},
                            {"path": "b.txt", "base_revision": stale, "hunks": [{"old": "beta", "new": "BETA"}]}
                        ]
                    }),
                )
                .call
                .arguments,
                None,
                CancellationToken::new(),
            )
            .await;
        let ToolOutcome::Value(output) = result.unwrap() else {
            panic!("one stale file must refuse the whole patch");
        };
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::StaleRevision)
        );
        assert!(
            output.model_content.contains("stale_revision"),
            "one stale file must refuse the whole patch: {}",
            output.model_content
        );

        // No side effects anywhere: the good file was not touched either.
        assert_eq!(
            tfs::read_to_string(dir.path().join("a.txt")).await.unwrap(),
            "alpha\n"
        );
        assert_eq!(
            tfs::read_to_string(dir.path().join("b.txt")).await.unwrap(),
            "BETA by someone else\n"
        );
        assert!(
            !dir.path().join(".focus-agent/changes.jsonl").exists(),
            "nothing is staged when the call refuses"
        );
    }
}

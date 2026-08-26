//! `edit.patch` — multi-hunk, revision-checked text patching.
//!
//! The file-revision companion to `edit.replace`: the canonical
//! model-visible shape applies exact-match hunks through one `files[]` batch,
//! including a path and `fs.read` revision for every entry. The parser keeps
//! the older top-level single-file shortcut for wire compatibility, but does
//! not advertise two competing shapes to the model. When a revision is
//! given, the file must still be exactly that revision, so an edit based on
//! stale content is refused instead of silently applying to drifted text.
//! Like `edit.replace`, the new content is staged as a prepared effect and
//! journaled; the runtime commits it behind the generation fence.

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, Effect, RunId, ToolFailureClass, ToolOutcome,
    ToolOutput, ToolRisk, ToolSemanticRole, ToolSpec, tool_failure_output,
};
use agent_workspace::{MAX_MUTATION_BYTES, MutationTransaction, Workspace};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    EDIT_ECHO_MAX_CHARS, ExactMatchError, LineEnding, Tool, adapt_edit_replacement,
    bound_chars_with_marker, candidate_regions, content_digest, edit_echo, exact_edit_match,
    hidden_path_output, is_not_found_error, missing_path_output, model_json_string,
    normalize_edit_line_endings, ordinary_view_blocked, projected_replacement_len,
};

const MAX_FILE_BYTES: usize = MAX_MUTATION_BYTES;
const MAX_HUNKS: usize = 64;
const MAX_FILES: usize = 16;

async fn rollback_staged_effects(effects: Vec<Box<dyn Effect>>, reason: &str) -> AgentResult<()> {
    // Roll back in reverse staging order. This is not required for file
    // correctness, but mirrors stack unwinding and keeps journal inspection
    // intuitive when a later file fails to prepare.
    let mut detail = String::new();
    let mut failed = 0usize;
    for effect in effects.into_iter().rev() {
        if let Err(error) = effect.rollback(reason).await {
            failed = failed.saturating_add(1);
            append_bounded_rollback_detail(&mut detail, &error.to_string());
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        let message = format!("{failed} staged edit rollback(s) could not be confirmed: {detail}");
        Err(AgentError::RecoveryRequired(bound_diagnostic(&message)))
    }
}

fn append_bounded_rollback_detail(target: &mut String, detail: &str) {
    let limit = agent_contracts::MAX_OPERATION_DIAGNOSTIC_BYTES;
    if target.len() >= limit {
        return;
    }
    if !target.is_empty() {
        if target.len().saturating_add(2) > limit {
            return;
        }
        target.push_str("; ");
    }
    let remaining = limit.saturating_sub(target.len());
    let mut end = detail.len().min(remaining);
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&detail[..end]);
}

fn bound_diagnostic(message: &str) -> String {
    let mut end = message
        .len()
        .min(agent_contracts::MAX_OPERATION_DIAGNOSTIC_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

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

#[derive(Deserialize, Clone, Copy, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HunkOp {
    #[default]
    Replace,
    InsertBefore,
    InsertAfter,
}

#[derive(Deserialize, Clone)]
struct Hunk {
    /// Parser compatibility defaults legacy `old`/`new` calls to replace.
    /// The model-visible schema requires an explicit operation so additions
    /// do not have to masquerade as destructive replacements.
    #[serde(default)]
    op: HunkOp,
    old: String,
    new: String,
    /// Parser-only compatibility with the earlier model surface. New model
    /// calls must use a unique exact anchor instead of a positional count:
    /// ordinal matches are brittle after preceding hunks move repeated text.
    #[serde(default)]
    occurrence: Option<usize>,
}

/// Resolve the two wire shapes without silently dropping revision intent.
/// Models commonly combine a one-entry `files[]` with the top-level
/// `base_revision` shown beside the shortcut fields. Binding that value is
/// safe for exactly one file; for a multi-file call it would be ambiguous.
fn resolved_files(args: &PatchArgs) -> AgentResult<Vec<FilePatch>> {
    match (&args.files, &args.path) {
        (files, None) if !files.is_empty() => {
            if !args.hunks.is_empty() {
                return Err(AgentError::InvalidRequest(
                    "edit.patch: top-level `hunks` cannot be combined with `files`".into(),
                ));
            }
            if files.len() > 1 && args.base_revision.is_some() {
                return Err(AgentError::InvalidRequest(
                    "edit.patch: top-level `base_revision` is ambiguous for multiple files; pass one revision per files[] entry".into(),
                ));
            }
            let mut files = files.clone();
            if let Some(top_revision) = args.base_revision.as_ref() {
                let file = &mut files[0];
                match file.base_revision.as_ref() {
                    Some(file_revision) if file_revision != top_revision => {
                        return Err(AgentError::InvalidRequest(
                            "edit.patch: conflicting top-level and files[0] base_revision values"
                                .into(),
                        ));
                    }
                    Some(_) => {}
                    None => file.base_revision = Some(top_revision.clone()),
                }
            }
            Ok(files)
        }
        (files, Some(path)) if files.is_empty() => Ok(vec![FilePatch {
            path: path.clone(),
            base_revision: args.base_revision.clone(),
            hunks: args.hunks.clone(),
        }]),
        (_, Some(_)) => Err(AgentError::InvalidRequest(
            "edit.patch: use either `files` or `path`+`hunks`, not both".into(),
        )),
        (_, None) => Err(AgentError::InvalidRequest(
            "edit.patch requires either `files` or `path`+`hunks`".into(),
        )),
    }
}

/// One resolved file patch: the confined path plus the content the hunks
/// will transform and the arguments that produced it.
struct ResolvedPatch {
    transaction: MutationTransaction,
    relative: String,
    original: String,
    updated: String,
    bytes_before: u64,
    hunks: usize,
    line_ending_before: LineEnding,
    line_ending_after: LineEnding,
    line_endings_normalized: bool,
}

impl EditPatchTool {
    /// Apply one exact-match hunk to the working text. The hunk's `old`
    /// must match exactly once on the current model surface. Parser-only
    /// compatibility still accepts a valid legacy `occurrence`.
    fn apply_hunk(
        original: &mut String,
        op: HunkOp,
        old: &str,
        new: &str,
        occurrence: Option<usize>,
        line_ending: LineEnding,
    ) -> Result<(usize, bool), ApplyHunkError> {
        let found = exact_edit_match(original, old, line_ending, occurrence)
            .map_err(ApplyHunkError::Match)?;
        let matched_eol_adapted = &original[found.start..found.end] != old;
        let replacement = adapt_edit_replacement(original, found, new, line_ending);
        let replacement_eol_adapted = replacement.as_ref() != new;
        let (start, end) = match op {
            HunkOp::Replace => (found.start, found.end),
            HunkOp::InsertBefore => (found.start, found.start),
            HunkOp::InsertAfter => (found.end, found.end),
        };
        let projected =
            projected_replacement_len(original.len(), end - start, replacement.len(), 1)
                .filter(|size| *size <= MAX_FILE_BYTES)
                .ok_or(ApplyHunkError::ResultTooLarge)?;
        if projected > original.capacity() {
            original.reserve(projected - original.len());
        }
        original.replace_range(start..end, replacement.as_ref());
        Ok((found.count, matched_eol_adapted || replacement_eol_adapted))
    }
}

enum ApplyHunkError {
    Match(ExactMatchError),
    ResultTooLarge,
}

#[async_trait]
impl Tool for EditPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit.patch".into(),
            description: "Exact revision-checked replace/insert hunks across files[]; max 16 files; max 64 hunks total.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["files"],
                "additionalProperties": false,
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                        "description": "The only model-visible form; use one entry for a single-file edit and at most 64 hunks total",
                        "items": {
                            "type": "object",
                            "required": ["path", "base_revision", "hunks"],
                            "additionalProperties": false,
                            "properties": {
                                "path": {"type": "string", "minLength": 1, "maxLength": 512},
                                "base_revision": {"type": "string", "pattern": "^[0-9a-f]{64}$", "description": "Exact `revision` from the latest fs.read of this path"},
                                "hunks": {
                                    "type": "array",
                                    "minItems": 1,
                                    "maxItems": 64,
                                    "items": {
                                        "type": "object",
                                        "required": ["op", "old", "new"],
                                        "additionalProperties": false,
                                        "properties": {
                                            "op": {"type": "string", "enum": ["replace", "insert_before", "insert_after"], "description": "replace removes the anchor; insert_before/insert_after preserve it. Use insert operations for additions."},
                                            "old": {"type": "string", "minLength": 1, "description": "Unique exact anchor; include enough unchanged context to disambiguate repeated text (only LF/CRLF encoding is token-equivalent)"},
                                            "new": {"type": "string", "description": "Replacement or inserted text; include every intended separator/newline explicitly"}
                                        }
                                    }
                                }
                            }
                        }
                    }
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
        let args: PatchArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("edit.patch args: {e}")))?;

        // Resolve the single-file shortcut and files[] form without
        // discarding an unambiguously supplied top-level revision.
        let files = resolved_files(&args)?;
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

        // Phase 1 — acquire every canonical path lease in sorted order, then
        // read one exact bounded snapshot per file. The returned transaction
        // owns those leases through composite settlement, so preflight and
        // staging operate on the same bytes without a second full read.
        for file in &files {
            if ordinary_view_blocked(&file.path) {
                return Ok(ToolOutcome::Value(hidden_path_output(
                    call_id,
                    "edit.patch",
                    &file.path,
                )));
            }
        }
        let requested: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        let snapshots = match self
            .workspace
            .begin_existing_mutations("edit.patch", "patch", &requested, MAX_FILE_BYTES)
            .await
        {
            Ok(snapshots) => snapshots,
            Err(error) if is_not_found_error(&error) => {
                // Batch acquisition intentionally reports one typed error.
                // Only on this failure path, locate the missing member so
                // the model receives the existing bounded path suggestions.
                for file in &files {
                    if self
                        .workspace
                        .confined_open_read(&file.path)
                        .await
                        .is_err_and(|candidate| is_not_found_error(&candidate))
                    {
                        return Ok(ToolOutcome::Value(
                            missing_path_output(&self.workspace, call_id, "edit.patch", &file.path)
                                .await,
                        ));
                    }
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };

        // Compute every file's new content before any mutation is staged, so
        // an invalid hunk or stale revision anywhere refuses the whole call
        // with no journal or filesystem side effect.
        let mut resolved: Vec<ResolvedPatch> = Vec::with_capacity(files.len());
        for (file, snapshot) in files.iter().zip(snapshots) {
            let relative = snapshot.relative_path().to_string();
            let current = snapshot.revision().to_string();
            let (transaction, original) = snapshot.into_parts();
            let original = String::from_utf8(original).map_err(|_| {
                AgentError::InvalidRequest(format!(
                    "edit.patch target is not UTF-8 text: {relative}"
                ))
            })?;

            // File-revision precondition: the model based its hunks on a
            // specific read; if the file moved on, the hunks may not apply
            // to what is actually there, so refuse instead of guessing.
            if let Some(expected) = &file.base_revision
                && current != *expected
            {
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

            let mut updated = original.clone();
            let original_line_ending = LineEnding::detect(&original);
            let mut line_endings_normalized = false;
            for hunk in &file.hunks {
                if hunk.old.is_empty() {
                    return Err(AgentError::InvalidRequest(
                        "edit.patch hunk `old` must not be empty".into(),
                    ));
                }
                // Every hunk sees the result of the previous hunk. Re-detect
                // the current style as part of that chaining contract: an
                // earlier hunk may introduce or remove the first newline.
                let current_line_ending = LineEnding::detect(&updated);
                let old = normalize_edit_line_endings(&hunk.old, current_line_ending);
                let new = normalize_edit_line_endings(&hunk.new, current_line_ending);
                line_endings_normalized |= old.as_ref() != hunk.old || new.as_ref() != hunk.new;
                match Self::apply_hunk(
                    &mut updated,
                    hunk.op,
                    old.as_ref(),
                    new.as_ref(),
                    hunk.occurrence,
                    current_line_ending,
                ) {
                    Ok((_, adapted)) => line_endings_normalized |= adapted,
                    Err(ApplyHunkError::ResultTooLarge) => {
                        return Err(AgentError::InvalidRequest(format!(
                            "edit.patch result exceeds the {MAX_FILE_BYTES}-byte file limit"
                        )));
                    }
                    Err(ApplyHunkError::Match(error)) => {
                        let (class, count, message) = match error {
                            ExactMatchError::Ambiguous { count } => (
                                ToolFailureClass::AmbiguousMatch,
                                count,
                                format!(
                                    "ambiguous_match: hunk `old` appears {count} times; include enough unchanged context to make the exact anchor unique"
                                ),
                            ),
                            ExactMatchError::NoMatch { count } => (
                                ToolFailureClass::NoExactMatch,
                                count,
                                format!(
                                    "no_exact_match: hunk `old` appears {count} times after target line-ending normalization. Matching stays exact."
                                ),
                            ),
                        };
                        return Ok(ToolOutcome::Value(patch_refusal(
                            call_id,
                            class,
                            &relative,
                            current,
                            &updated,
                            old.as_ref(),
                            &message,
                            count,
                        )));
                    }
                }
            }
            let bytes_before = original.len() as u64;
            let line_ending_after = LineEnding::detect(&updated);
            resolved.push(ResolvedPatch {
                transaction,
                relative,
                original,
                updated,
                bytes_before,
                hunks: file.hunks.len(),
                line_ending_before: original_line_ending,
                line_ending_after,
                line_endings_normalized,
            });
        }

        // Phase 2 — stage the changed files as prepared effects. Nothing
        // lands yet: the composite effect is committed by the runtime
        // behind the generation fence. File commits are sequential, not a
        // cross-file transaction; a later failure cleans unattempted staged
        // files and reports the earlier applied files as recovery work.
        //
        let mut effects: Vec<Box<dyn Effect>> = Vec::new();
        let mut file_reports: Vec<Value> = Vec::new();
        let mut echo_blocks: Vec<String> = Vec::new();
        let mut revisions_in_files_order: Vec<String> = Vec::with_capacity(resolved.len());
        let mut changed_any = false;
        let mut total_hunks_applied = 0usize;
        for (file_index, patch) in resolved.iter().enumerate() {
            let changed = patch.updated != patch.original;
            let revision = content_digest(patch.updated.as_bytes());
            revisions_in_files_order.push(revision.clone());
            file_reports.push(json!({
                "path": patch.relative,
                "changed": changed,
                "hunks": if changed { patch.hunks } else { 0 },
                "bytes_before": patch.bytes_before,
                "bytes_after": patch.updated.len(),
                // The revision of the applied result, so a follow-up
                // patch can chain on it.
                "revision": revision,
                // `line_ending` describes `revision`; the former style is
                // retained explicitly for diagnostics.
                "line_ending_before": patch.line_ending_before.as_str(),
                "line_ending": patch.line_ending_after.as_str(),
                "line_endings_normalized": patch.line_endings_normalized,
            }));
            if changed {
                changed_any = true;
                total_hunks_applied += patch.hunks;
                // Bounded echo of the changed region in the *updated*
                // content: the model can chain the next hunk (or pass
                // `base_revision`) on what the file looks like now,
                // without a confirm `fs.read` per file.
                echo_blocks.push(format!(
                    "--- file[{file_index}] path={}\n{}",
                    model_json_string(&patch.relative),
                    edit_echo(&patch.original, &patch.updated, EDIT_ECHO_MAX_CHARS).trim_end()
                ));
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

        for patch in resolved
            .into_iter()
            .filter(|patch| patch.updated != patch.original)
        {
            // The new content is staged and journaled as prepared; the
            // atomic rename (the side effect) is committed by the runtime
            // after the generation fence.
            let ResolvedPatch {
                transaction,
                updated,
                ..
            } = patch;
            let prepared = match effect_context.clone() {
                Some(context) => {
                    transaction
                        .prepare_with_effect_context(updated.as_bytes(), context)
                        .await
                }
                None => transaction.prepare(updated.as_bytes()).await,
            };
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    if let Err(rollback_error) = rollback_staged_effects(
                        effects,
                        "edit.patch staging aborted after a later file failed to prepare",
                    )
                    .await
                    {
                        return Err(AgentError::RecoveryRequired(bound_diagnostic(&format!(
                            "edit.patch staging failed ({error}); prior staged cleanup failed ({rollback_error})"
                        ))));
                    }
                    return Err(error);
                }
            };
            effects.push(Box::new(prepared));
        }

        let echo = bound_chars_with_marker(
            &echo_blocks.join("\n"),
            EDIT_ECHO_MAX_CHARS,
            "\n… (middle edit echoes omitted at global cap; use fs.read for full bodies)\n",
        );
        // Keep every new revision outside the optional echo budget. The list
        // is in the same order as the submitted `files[]`; with at most 16
        // SHA-256 values it is bounded to roughly 1.1 KiB and cannot lose a
        // later file merely because earlier changed-region previews are long.
        let revision_manifest = revisions_in_files_order
            .iter()
            .enumerate()
            .map(|(index, revision)| format!("{index}:{revision}"))
            .collect::<Vec<_>>()
            .join(",");
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
                    "patch applied: {} file(s), {} hunk(s)\nrevisions_in_files_order={}\n{}",
                    effects.len(),
                    total_hunks_applied,
                    revision_manifest,
                    echo
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
    let quoted_path = model_json_string(path);
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
        format!("{message}\npath={quoted_path}\nrevision={revision}\n{candidate_text}"),
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
    use agent_contracts::{CancellationToken, EffectReceipt, ToolExecutionRequest};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::fs as tfs;

    struct RollbackProbe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Effect for RollbackProbe {
        fn describe(&self) -> String {
            "rollback probe".into()
        }

        async fn commit(self: Box<Self>) -> EffectReceipt {
            panic!("rollback probe must not commit")
        }

        async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FailingRollbackProbe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl Effect for FailingRollbackProbe {
        fn describe(&self) -> String {
            "failing rollback probe".into()
        }

        async fn commit(self: Box<Self>) -> EffectReceipt {
            panic!("rollback probe must not commit")
        }

        async fn rollback(self: Box<Self>, _reason: &str) -> AgentResult<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(AgentError::RecoveryRequired(
                "simulated staged cleanup failure".into(),
            ))
        }
    }

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
    async fn model_schema_exposes_one_revision_checked_files_shape() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let spec = EditPatchTool::new(workspace).spec();
        assert!(spec.description.chars().count() <= 96);
        assert!(spec.description.contains("max 64 hunks total"));
        let schema = spec.input_schema;

        assert_eq!(schema["required"], json!(["files"]));
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"].get("path").is_none());
        assert!(schema["properties"].get("base_revision").is_none());
        assert!(schema["properties"].get("hunks").is_none());
        assert_eq!(
            schema["properties"]["files"]["items"]["required"],
            json!(["path", "base_revision", "hunks"])
        );
        let file = &schema["properties"]["files"]["items"]["properties"];
        assert_eq!(file["path"]["maxLength"], 512);
        assert_eq!(file["base_revision"]["pattern"], "^[0-9a-f]{64}$");
        let hunk = &file["hunks"]["items"];
        assert_eq!(hunk["required"], json!(["op", "old", "new"]));
        assert_eq!(
            hunk["properties"]["op"]["enum"],
            json!(["replace", "insert_before", "insert_after"])
        );
        assert_eq!(hunk["properties"]["old"]["minLength"], 1);
        assert!(
            hunk["properties"].get("occurrence").is_none(),
            "ordinal selection is parser-only compatibility, not model surface"
        );
    }

    #[tokio::test]
    async fn legacy_occurrence_remains_parser_compatible_but_off_surface() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "same\nsame\n").await.unwrap();
        let revision = read_revision(&workspace, "f.txt").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "legacy",
                json!({
                    "files": [{
                        "path": "f.txt",
                        "base_revision": revision,
                        "hunks": [{"old": "same", "new": "changed", "occurrence": 2}]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("legacy occurrence must remain executable through the parser");
        };
        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Applied { .. }
        ));
        assert_eq!(tfs::read_to_string(file).await.unwrap(), "same\nchanged\n");
    }

    #[tokio::test]
    async fn staging_abort_rolls_back_every_prepared_sibling() {
        let rolled_back = Arc::new(AtomicUsize::new(0));
        let effects: Vec<Box<dyn Effect>> = (0..3)
            .map(|_| Box::new(RollbackProbe(rolled_back.clone())) as Box<dyn Effect>)
            .collect();

        rollback_staged_effects(effects, "later prepare failed")
            .await
            .unwrap();
        assert_eq!(rolled_back.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn staging_abort_attempts_every_sibling_and_propagates_cleanup_failure() {
        let rolled_back = Arc::new(AtomicUsize::new(0));
        let effects: Vec<Box<dyn Effect>> = vec![
            Box::new(RollbackProbe(rolled_back.clone())),
            Box::new(FailingRollbackProbe(rolled_back.clone())),
            Box::new(RollbackProbe(rolled_back.clone())),
        ];

        let error = rollback_staged_effects(effects, "later prepare failed")
            .await
            .unwrap_err();
        assert!(
            matches!(error, AgentError::RecoveryRequired(message) if message.contains("simulated staged cleanup failure"))
        );
        assert_eq!(rolled_back.load(Ordering::SeqCst), 3);
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
        assert!(
            output.heats_working_set(),
            "patch files[] must be trusted ResourceTouches"
        );
        assert_eq!(output.metadata["files"][0]["path"], "lib.rs");
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
    async fn explicit_insert_hunks_preserve_their_unique_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("lib.rs");
        tfs::write(&file, "fn first() {}\nfn last() {}\n")
            .await
            .unwrap();
        let revision = read_revision(&workspace, "lib.rs").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "insert",
                json!({
                    "files": [{
                        "path": "lib.rs",
                        "base_revision": revision,
                        "hunks": [
                            {
                                "op": "insert_after",
                                "old": "fn first() {}",
                                "new": "\nfn second() {}"
                            },
                            {
                                "op": "insert_before",
                                "old": "fn last() {}",
                                "new": "fn penultimate() {}\n"
                            }
                        ]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("explicit insert hunks must prepare a committed effect");
        };
        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read_to_string(file).await.unwrap(),
            "fn first() {}\nfn second() {}\nfn penultimate() {}\nfn last() {}\n"
        );
    }

    #[tokio::test]
    async fn explicit_insert_adapts_newlines_without_removing_a_crlf_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("lib.rs");
        tfs::write(&file, "fn first() {}\r\nfn last() {}\r\n")
            .await
            .unwrap();
        let revision = read_revision(&workspace, "lib.rs").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "insert-crlf",
                json!({
                    "files": [{
                        "path": "lib.rs",
                        "base_revision": revision,
                        "hunks": [{
                            "op": "insert_after",
                            "old": "fn first() {}",
                            "new": "\nfn middle() {}"
                        }]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("CRLF insert must prepare a committed effect");
        };
        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read(file).await.unwrap(),
            b"fn first() {}\r\nfn middle() {}\r\nfn last() {}\r\n"
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
    async fn top_revision_binds_exactly_one_files_entry() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "one\n").await.unwrap();
        let revision = read_revision(&workspace, "f.txt").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "base_revision": revision,
                    "files": [{
                        "path": "f.txt",
                        "hunks": [{"old": "one", "new": "two"}]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { effect, .. } = outcome else {
            panic!("an unambiguous top revision must bind the one files[] entry");
        };
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                ..
            }
        ));
        assert_eq!(tfs::read_to_string(file).await.unwrap(), "two\n");
    }

    #[tokio::test]
    async fn stale_top_revision_cannot_be_silently_dropped_from_one_file_entry() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("f.txt");
        tfs::write(&file, "one\n").await.unwrap();
        let stale_revision = read_revision(&workspace, "f.txt").await;
        tfs::write(&file, "changed elsewhere\n").await.unwrap();
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "base_revision": stale_revision,
                    "files": [{
                        "path": "f.txt",
                        "hunks": [{"old": "one", "new": "two"}]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("a stale compatibility revision must refuse before staging");
        };
        assert_eq!(
            output.failure_class(),
            Some(ToolFailureClass::StaleRevision)
        );
        assert_eq!(
            tfs::read_to_string(file).await.unwrap(),
            "changed elsewhere\n"
        );
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn ambiguous_or_conflicting_top_revision_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tfs::write(dir.path().join("a.txt"), "a\n").await.unwrap();
        tfs::write(dir.path().join("b.txt"), "b\n").await.unwrap();
        let a_revision = read_revision(&workspace, "a.txt").await;
        let b_revision = read_revision(&workspace, "b.txt").await;
        let tool = EditPatchTool::new(workspace);

        let conflict = tool
            .execute(
                RunId::new(),
                "c1",
                json!({
                    "base_revision": a_revision.clone(),
                    "files": [{
                        "path": "a.txt",
                        "base_revision": b_revision,
                        "hunks": [{"old": "a", "new": "A"}]
                    }]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(conflict, AgentError::InvalidRequest(message) if message.contains("conflicting"))
        );

        let ambiguous = tool
            .execute(
                RunId::new(),
                "c2",
                json!({
                    "base_revision": a_revision,
                    "files": [
                        {"path": "a.txt", "hunks": [{"old": "a", "new": "A"}]},
                        {"path": "b.txt", "hunks": [{"old": "b", "new": "B"}]}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(ambiguous, AgentError::InvalidRequest(message) if message.contains("ambiguous"))
        );
        assert_eq!(
            tfs::read_to_string(dir.path().join("a.txt")).await.unwrap(),
            "a\n"
        );
        assert_eq!(
            tfs::read_to_string(dir.path().join("b.txt")).await.unwrap(),
            "b\n"
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

    #[tokio::test]
    async fn lf_hunks_apply_in_order_to_crlf_without_mixing_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("windows.txt");
        tfs::write(&file, b"one\r\ntwo\r\nthree\r\n").await.unwrap();
        let revision = read_revision(&workspace, "windows.txt").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "windows.txt",
                    "base_revision": revision,
                    "hunks": [
                        {"old": "one\ntwo", "new": "ONE\nTWO"},
                        {"old": "TWO\nthree", "new": "two\nTHREE"}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("LF hunks must match a uniform CRLF target");
        };
        assert_eq!(output.metadata["files"][0]["line_ending_before"], "crlf");
        assert_eq!(output.metadata["files"][0]["line_ending"], "crlf");
        assert_eq!(output.metadata["files"][0]["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(tfs::read(&file).await.unwrap(), b"ONE\r\ntwo\r\nTHREE\r\n");
    }

    #[tokio::test]
    async fn patch_lone_cr_cannot_split_uniform_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("windows.txt");
        let original = b"a\r\nb\r\n";
        tfs::write(&file, original).await.unwrap();
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "windows.txt",
                    "hunks": [{"old": "\r", "new": "X"}]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::Value(output) = outcome else {
            panic!("a lone CR hunk must refuse instead of splitting CRLF");
        };
        assert_eq!(output.failure_class(), Some(ToolFailureClass::NoExactMatch));
        assert_eq!(tfs::read(&file).await.unwrap(), original);
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn mixed_eol_hunks_chain_on_updated_logical_text() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("mixed.txt");
        tfs::write(&file, b"one\r\ntwo\nthree\r\nfour\n")
            .await
            .unwrap();
        let revision = read_revision(&workspace, "mixed.txt").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "mixed.txt",
                    "base_revision": revision,
                    "hunks": [
                        {"old": "one\ntwo\nthree", "new": "ONE\nTWO\nTHREE"},
                        {"old": "THREE\nfour", "new": "three\nFOUR"}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("mixed-EOL hunks must use logical newline exact matching");
        };
        assert_eq!(output.metadata["files"][0]["line_ending"], "mixed");
        assert_eq!(output.metadata["files"][0]["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(
            tfs::read(&file).await.unwrap(),
            b"ONE\r\nTWO\nthree\r\nFOUR\n"
        );
    }

    #[tokio::test]
    async fn later_hunk_uses_line_endings_introduced_by_earlier_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("chain.txt");
        tfs::write(&file, b"seed").await.unwrap();
        let revision = read_revision(&workspace, "chain.txt").await;
        let tool = EditPatchTool::new(workspace);

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "path": "chain.txt",
                    "base_revision": revision,
                    "hunks": [
                        {"old": "seed", "new": "one\nTWO"},
                        {"old": "one\r\nTWO", "new": "done\r\nnext"}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("later hunks must observe line endings introduced by earlier hunks");
        };
        assert_eq!(output.metadata["files"][0]["line_ending_before"], "none");
        assert_eq!(output.metadata["files"][0]["line_ending"], "lf");
        assert_eq!(output.metadata["files"][0]["line_endings_normalized"], true);
        assert!(matches!(
            effect.commit().await,
            agent_contracts::EffectReceipt::Applied { .. }
        ));
        assert_eq!(tfs::read(&file).await.unwrap(), b"done\nnext");
    }

    #[tokio::test]
    async fn duplicate_resolved_target_is_rejected_before_staging() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let file = dir.path().join("same.txt");
        tfs::write(&file, "alpha beta\n").await.unwrap();
        let tool = EditPatchTool::new(workspace);

        let result = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "files": [
                        {"path": "same.txt", "hunks": [{"old": "alpha", "new": "A"}]},
                        {"path": "./same.txt", "hunks": [{"old": "beta", "new": "B"}]}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err(), "one target may appear only once per patch");
        assert_eq!(tfs::read_to_string(&file).await.unwrap(), "alpha beta\n");
        assert!(!dir.path().join(".focus-agent/changes.jsonl").exists());
    }

    #[tokio::test]
    async fn multi_file_success_echo_has_one_global_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        tfs::write(dir.path().join("a.txt"), "a\n").await.unwrap();
        tfs::write(dir.path().join("b.txt"), "b\n").await.unwrap();
        let tool = EditPatchTool::new(workspace);
        let long_a = (0..40)
            .map(|index| format!("A-{index:02} padding padding padding\n"))
            .collect::<String>();
        let long_b = (0..40)
            .map(|index| format!("B-{index:02} padding padding padding\n"))
            .collect::<String>();

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({
                    "files": [
                        {"path": "a.txt", "hunks": [{"old": "a\n", "new": long_a}]},
                        {"path": "b.txt", "hunks": [{"old": "b\n", "new": long_b}]}
                    ]
                }),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("both files should stage");
        };
        let mut sections = output.model_content.splitn(3, '\n');
        let _summary = sections.next().unwrap();
        let revisions = sections.next().unwrap();
        let echo = sections.next().unwrap();
        assert!(revisions.starts_with("revisions_in_files_order=0:"));
        assert!(revisions.contains(",1:"));
        assert!(
            echo.chars().count() <= EDIT_ECHO_MAX_CHARS,
            "all file echoes share one hard cap: {}",
            echo.chars().count()
        );
        assert!(
            echo.contains("A-00"),
            "the first file head stays visible: {echo}"
        );
        assert!(
            echo.contains("B-39"),
            "the final file tail stays visible: {echo}"
        );
        assert!(
            echo.contains("middle edit echoes omitted"),
            "global truncation must identify the omitted middle: {echo}"
        );
        effect.rollback("test cleanup").await.unwrap();
    }

    #[tokio::test]
    async fn sixteen_file_success_keeps_every_revision_outside_the_echo_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let tool = EditPatchTool::new(workspace);
        let mut files = Vec::new();
        let mut expected_revisions = Vec::new();
        for index in 0..MAX_FILES {
            let path = format!("{index:x}");
            let old = format!("old-{index}\n");
            let new = format!("new-{index}\n");
            tfs::write(dir.path().join(&path), old.as_bytes())
                .await
                .unwrap();
            expected_revisions.push(content_digest(new.as_bytes()));
            files.push(json!({
                "path": path,
                "hunks": [{"old": old, "new": new}],
            }));
        }

        let outcome = tool
            .execute(
                RunId::new(),
                "c",
                json!({"files": files}),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let ToolOutcome::PreparedEffect { output, effect } = outcome else {
            panic!("all files should stage");
        };
        let manifest = output.model_content.lines().nth(1).unwrap();
        for (index, revision) in expected_revisions.iter().enumerate() {
            assert!(
                manifest.contains(&format!("{index}:{revision}")),
                "revision {index} must survive regardless of echo truncation"
            );
        }
        assert!(
            output.model_content.chars().count() <= 2 * EDIT_ECHO_MAX_CHARS,
            "revision manifest plus globally bounded echo stays small"
        );
        effect.rollback("test cleanup").await.unwrap();
    }
}
